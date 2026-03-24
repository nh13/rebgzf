use crate::bits::BitWriter;
use crate::deflate::tables::{encode_distance, encode_length};
use crate::deflate::tokens::LZ77Token;
use crate::huffman::HuffmanEncoder;

/// Maximum LZ77 back-reference distance (32KB).
const MAX_DISTANCE: usize = 32768;

/// Resolves LZ77 back-references that cross BGZF block boundaries.
///
/// Uses a linear decode buffer instead of a circular sliding window.
/// The buffer holds `[prev_tail: up to 32KB] [current_block: growing]`,
/// enabling:
/// - Simple array indexing for Copy lookups (no circular wrapping)
/// - Single contiguous CRC hash per block (enables SIMD in crc32fast)
/// - Bulk memcpy for non-RLE Copies
pub struct BoundaryResolver {
    /// Linear buffer: previous block tail + current block's decoded bytes.
    /// Layout: `[0..tail_len] = previous block's last 32KB`
    ///         `[tail_len..tail_len+current_len] = current block's bytes`
    decode_buf: Vec<u8>,
    /// Length of the previous-block tail portion (0..=32768)
    tail_len: usize,
    /// Current block's decoded byte count (bytes written past tail_len)
    current_len: usize,
    /// Current position in the uncompressed stream
    position: u64,
    /// Statistics
    refs_resolved: u64,
    refs_preserved: u64,
}

impl BoundaryResolver {
    pub fn new() -> Self {
        // Pre-allocate for typical BGZF block: 32KB tail + ~72KB block
        Self {
            decode_buf: Vec::with_capacity(MAX_DISTANCE + 72 * 1024),
            tail_len: 0,
            current_len: 0,
            position: 0,
            refs_resolved: 0,
            refs_preserved: 0,
        }
    }

    /// Append a byte to the current block region of the decode buffer.
    #[inline(always)]
    fn push_byte(&mut self, byte: u8) {
        self.decode_buf.push(byte);
        self.current_len += 1;
    }

    /// Copy `length` bytes from `distance` bytes back in the decode buffer.
    /// Handles both non-RLE (distance >= length) and RLE (distance < length) cases.
    #[inline]
    fn copy_from_back(&mut self, distance: u16, length: u16) {
        let dist = distance as usize;
        let len = length as usize;
        let src_start = self.decode_buf.len() - dist;

        if dist >= len {
            // Non-RLE: source doesn't overlap destination, bulk copy
            // We must extend first, then copy (can't extend_from_within for overlapping)
            let old_len = self.decode_buf.len();
            self.decode_buf.resize(old_len + len, 0);
            self.decode_buf.copy_within(src_start..src_start + len, old_len);
        } else {
            // RLE: source overlaps destination, byte-by-byte with pattern repeat
            self.decode_buf.reserve(len);
            for i in 0..len {
                let byte = self.decode_buf[src_start + (i % dist)];
                self.decode_buf.push(byte);
            }
        }
        self.current_len += len;
    }

    /// Prepare for the next block: shift the last 32KB of decoded data
    /// to become the tail for the next block's lookups.
    fn rotate_tail(&mut self) {
        let total = self.decode_buf.len();
        let keep = total.min(MAX_DISTANCE);
        if keep < total {
            // Shift the last `keep` bytes to the front
            self.decode_buf.copy_within(total - keep..total, 0);
            self.decode_buf.truncate(keep);
        }
        self.tail_len = keep;
        self.current_len = 0;
    }

    /// Process tokens for a BGZF block, resolving cross-boundary LZ77 references.
    ///
    /// `block_start`: position where this BGZF block starts
    /// `tokens`: LZ77 tokens to process
    ///
    /// Returns: (tokens with cross-boundary references resolved, CRC32, uncompressed size)
    pub fn resolve_block(
        &mut self,
        block_start: u64,
        tokens: &[LZ77Token],
    ) -> (Vec<LZ77Token>, u32, u32) {
        let mut output = Vec::with_capacity(tokens.len());

        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    self.push_byte(*byte);
                    self.position += 1;
                    output.push(LZ77Token::Literal(*byte));
                }

                LZ77Token::Copy { length, distance } => {
                    let ref_start = self.position.saturating_sub(*distance as u64);

                    if ref_start < block_start {
                        // Cross-boundary: resolve to literals
                        let dist = *distance as usize;
                        let len = *length as usize;
                        let src_start = self.decode_buf.len() - dist;
                        for i in 0..len {
                            let byte = self.decode_buf[src_start + (i % dist)];
                            self.push_byte(byte);
                            output.push(LZ77Token::Literal(byte));
                        }
                        self.refs_resolved += 1;
                    } else {
                        // Within-block: preserve Copy, append decoded bytes
                        self.copy_from_back(*distance, *length);
                        output.push(LZ77Token::Copy { length: *length, distance: *distance });
                        self.refs_preserved += 1;
                    }

                    self.position += *length as u64;
                }

                LZ77Token::EndOfBlock => {}
            }
        }

        // CRC over the current block's contiguous bytes — one SIMD-friendly call
        let block_bytes = &self.decode_buf[self.tail_len..self.tail_len + self.current_len];
        let crc = crc32fast::hash(block_bytes);
        let uncompressed_size = self.current_len as u32;

        // Rotate: keep last 32KB as tail for next block
        self.rotate_tail();

        (output, crc, uncompressed_size)
    }

    /// Fused resolve + encode for fixed Huffman (single-threaded path).
    ///
    /// Resolves cross-boundary references AND encodes to DEFLATE in one pass,
    /// eliminating the intermediate `Vec<LZ77Token>` and the second token iteration.
    /// Only valid for fixed Huffman (levels 1-3) since dynamic requires a frequency pass.
    ///
    /// Returns: (DEFLATE bytes, CRC32, uncompressed size)
    pub fn resolve_and_encode_fixed(
        &mut self,
        block_start: u64,
        tokens: &[LZ77Token],
        encoder: &HuffmanEncoder,
    ) -> (Vec<u8>, u32, u32) {
        let mut writer = BitWriter::with_capacity(tokens.len() * 2);
        writer.write_bit(true); // BFINAL
        writer.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)

        let lit_codes = encoder.fixed_lit_codes();
        let dist_codes = encoder.fixed_dist_codes();

        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    self.push_byte(*byte);
                    self.position += 1;
                    // Encode literal directly
                    let (code, len) = lit_codes[*byte as usize];
                    writer.write_bits(code, len);
                }

                LZ77Token::Copy { length, distance } => {
                    let ref_start = self.position.saturating_sub(*distance as u64);

                    if ref_start < block_start {
                        // Cross-boundary: resolve to literals and encode each
                        let dist = *distance as usize;
                        let len = *length as usize;
                        let src_start = self.decode_buf.len() - dist;
                        for i in 0..len {
                            let byte = self.decode_buf[src_start + (i % dist)];
                            self.push_byte(byte);
                            let (code, code_len) = lit_codes[byte as usize];
                            writer.write_bits(code, code_len);
                        }
                        self.refs_resolved += 1;
                    } else {
                        // Within-block: encode as Copy
                        self.copy_from_back(*distance, *length);
                        if let Some((len_code, extra_val, extra_bits)) = encode_length(*length) {
                            let (code, code_len) = lit_codes[len_code as usize];
                            writer.write_bits(code, code_len);
                            if extra_bits > 0 {
                                writer.write_bits(extra_val as u32, extra_bits);
                            }
                        }
                        if let Some((dist_code, extra_val, extra_bits)) = encode_distance(*distance)
                        {
                            let (code, code_len) = dist_codes[dist_code as usize];
                            writer.write_bits(code, code_len);
                            if extra_bits > 0 {
                                writer.write_bits(extra_val as u32, extra_bits);
                            }
                        }
                        self.refs_preserved += 1;
                    }

                    self.position += *length as u64;
                }

                LZ77Token::EndOfBlock => {}
            }
        }

        // Write end-of-block symbol
        let (code, len) = lit_codes[256];
        writer.write_bits(code, len);

        let deflate_data = writer.finish();

        // CRC over the current block's contiguous bytes
        let block_bytes = &self.decode_buf[self.tail_len..self.tail_len + self.current_len];
        let crc = crc32fast::hash(block_bytes);
        let uncompressed_size = self.current_len as u32;

        self.rotate_tail();

        (deflate_data, crc, uncompressed_size)
    }

    /// Get the current position in uncompressed stream
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Get statistics (resolved, preserved)
    pub fn stats(&self) -> (u64, u64) {
        (self.refs_resolved, self.refs_preserved)
    }

    /// Reset the resolver
    pub fn reset(&mut self) {
        self.decode_buf.clear();
        self.tail_len = 0;
        self.current_len = 0;
        self.position = 0;
        self.refs_resolved = 0;
        self.refs_preserved = 0;
    }
}

impl Default for BoundaryResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literals_only() {
        let mut resolver = BoundaryResolver::new();

        let tokens = vec![LZ77Token::Literal(b'H'), LZ77Token::Literal(b'i')];
        let (resolved, crc, size) = resolver.resolve_block(0, &tokens);

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], LZ77Token::Literal(b'H'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'i'));
        assert_eq!(resolver.position(), 2);
        assert_eq!(size, 2);
        assert_eq!(crc, crc32fast::hash(b"Hi"));
    }

    #[test]
    fn test_copy_within_block() {
        let mut resolver = BoundaryResolver::new();

        // Block starts at 0, contains "ABAB" where second AB is a copy
        let tokens = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Copy { length: 2, distance: 2 }, // Copy "AB"
        ];
        let (resolved, crc, size) = resolver.resolve_block(0, &tokens);

        // Copy should be preserved since it references within block
        assert_eq!(resolved.len(), 3);
        assert!(matches!(resolved[2], LZ77Token::Copy { .. }));
        assert_eq!(size, 4);
        assert_eq!(crc, crc32fast::hash(b"ABAB"));

        let (refs_resolved, refs_preserved) = resolver.stats();
        assert_eq!(refs_resolved, 0);
        assert_eq!(refs_preserved, 1);
    }

    #[test]
    fn test_copy_crosses_boundary() {
        let mut resolver = BoundaryResolver::new();

        // First block: "ABCD"
        let tokens1 = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Literal(b'C'),
            LZ77Token::Literal(b'D'),
        ];
        let (_, crc1, size1) = resolver.resolve_block(0, &tokens1);
        assert_eq!(resolver.position(), 4);
        assert_eq!(size1, 4);
        assert_eq!(crc1, crc32fast::hash(b"ABCD"));

        // Second block starting at position 4
        // Contains a reference back to first block
        let tokens2 = vec![
            LZ77Token::Literal(b'E'),
            LZ77Token::Copy { length: 2, distance: 5 }, // refs "AB" in block 1
        ];
        let (resolved, crc2, size2) = resolver.resolve_block(4, &tokens2);

        // Copy should be resolved to literals since it references previous block
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0], LZ77Token::Literal(b'E'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'A'));
        assert_eq!(resolved[2], LZ77Token::Literal(b'B'));
        assert_eq!(size2, 3);
        assert_eq!(crc2, crc32fast::hash(b"EAB"));

        let (refs_resolved, refs_preserved) = resolver.stats();
        assert_eq!(refs_resolved, 1);
        assert_eq!(refs_preserved, 0);
    }

    #[test]
    fn test_mixed_copies() {
        let mut resolver = BoundaryResolver::new();

        // First block: "ABCD"
        let tokens1 = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Literal(b'C'),
            LZ77Token::Literal(b'D'),
        ];
        let _ = resolver.resolve_block(0, &tokens1);

        // Second block: "E" + copy from block 1 + copy within block 2
        let tokens2 = vec![
            LZ77Token::Literal(b'E'),
            LZ77Token::Copy { length: 2, distance: 5 }, // refs block 1 -> resolve
            LZ77Token::Copy { length: 2, distance: 1 }, // refs within block 2 -> preserve
        ];
        let (resolved, crc, size) = resolver.resolve_block(4, &tokens2);

        // Should have: E, A, B, Copy(2,1)
        assert_eq!(resolved.len(), 4);
        assert_eq!(resolved[0], LZ77Token::Literal(b'E'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'A'));
        assert_eq!(resolved[2], LZ77Token::Literal(b'B'));
        assert!(matches!(resolved[3], LZ77Token::Copy { length: 2, distance: 1 }));
        assert_eq!(size, 5);
        assert_eq!(crc, crc32fast::hash(b"EABBB")); // E + AB + BB (copy of last B twice)
    }
}
