use crate::bits::BitWriter;
use crate::deflate::tables::{encode_distance, encode_length};
use crate::deflate::tokens::LZ77Token;
use crate::error::Result;

/// Huffman encoder for DEFLATE output
pub struct HuffmanEncoder {
    use_fixed: bool,
    /// Fixed literal/length codes (precomputed)
    fixed_lit_codes: Vec<(u32, u8)>,
    /// Fixed distance codes (precomputed)
    fixed_dist_codes: Vec<(u32, u8)>,
}

impl HuffmanEncoder {
    pub fn new(use_fixed: bool) -> Self {
        let fixed_lit_codes = build_fixed_literal_codes();
        let fixed_dist_codes = build_fixed_distance_codes();

        Self { use_fixed, fixed_lit_codes, fixed_dist_codes }
    }

    /// Encode LZ77 tokens to DEFLATE format
    pub fn encode(&mut self, tokens: &[LZ77Token], is_final: bool) -> Result<Vec<u8>> {
        let mut writer = BitWriter::with_capacity(tokens.len() * 2);

        // Write block header
        writer.write_bit(is_final); // BFINAL
        if self.use_fixed {
            writer.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)
            self.encode_fixed(&mut writer, tokens)?;
        } else {
            // For now, always use fixed. Dynamic Huffman requires computing optimal tables.
            writer.write_bits(1, 2);
            self.encode_fixed(&mut writer, tokens)?;
        }

        Ok(writer.finish())
    }

    fn encode_fixed(&self, writer: &mut BitWriter, tokens: &[LZ77Token]) -> Result<()> {
        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    let (code, len) = self.fixed_lit_codes[*byte as usize];
                    writer.write_bits_reversed(code, len);
                }
                LZ77Token::Copy { length, distance } => {
                    // Encode length
                    if let Some((len_code, extra_val, extra_bits)) = encode_length(*length) {
                        let (code, code_len) = self.fixed_lit_codes[len_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }

                    // Encode distance
                    if let Some((dist_code, extra_val, extra_bits)) = encode_distance(*distance) {
                        let (code, code_len) = self.fixed_dist_codes[dist_code as usize];
                        writer.write_bits_reversed(code, code_len);
                        if extra_bits > 0 {
                            writer.write_bits(extra_val as u32, extra_bits);
                        }
                    }
                }
                LZ77Token::EndOfBlock => {
                    // Symbol 256 = end of block
                    let (code, len) = self.fixed_lit_codes[256];
                    writer.write_bits_reversed(code, len);
                }
            }
        }

        // Always write end of block
        let (code, len) = self.fixed_lit_codes[256];
        writer.write_bits_reversed(code, len);

        Ok(())
    }
}

/// Build fixed Huffman codes for literals/lengths (RFC 1951 section 3.2.6)
fn build_fixed_literal_codes() -> Vec<(u32, u8)> {
    let lengths = super::tables::fixed_literal_lengths();
    build_codes_from_lengths(&lengths)
}

/// Build fixed Huffman codes for distances
fn build_fixed_distance_codes() -> Vec<(u32, u8)> {
    let lengths = super::tables::fixed_distance_lengths();
    build_codes_from_lengths(&lengths)
}

/// Build canonical Huffman codes from code lengths
fn build_codes_from_lengths(lengths: &[u8]) -> Vec<(u32, u8)> {
    let max_bits = *lengths.iter().max().unwrap_or(&0);

    // Count codes of each length
    let mut bl_count = vec![0u32; max_bits as usize + 1];
    for &len in lengths {
        if len > 0 {
            bl_count[len as usize] += 1;
        }
    }

    // Compute first code for each bit length
    let mut next_code = vec![0u32; max_bits as usize + 1];
    let mut code = 0u32;
    for bits in 1..=max_bits as usize {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }

    // Assign codes to symbols
    let mut codes = vec![(0u32, 0u8); lengths.len()];
    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 {
            codes[sym] = (next_code[len as usize], len);
            next_code[len as usize] += 1;
        }
    }

    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_fixed_literal_codes() {
        let codes = build_fixed_literal_codes();
        assert_eq!(codes.len(), 288);

        // Check some known codes (RFC 1951 section 3.2.6)
        // Symbols 0-143: 8 bits, codes 00110000 - 10111111
        assert_eq!(codes[0].1, 8); // 8-bit code
        assert_eq!(codes[143].1, 8); // 8-bit code

        // Symbols 144-255: 9 bits, codes 110010000 - 111111111
        assert_eq!(codes[144].1, 9);
        assert_eq!(codes[255].1, 9);

        // Symbols 256-279: 7 bits, codes 0000000 - 0010111
        assert_eq!(codes[256].1, 7); // End of block
        assert_eq!(codes[279].1, 7);

        // Symbols 280-287: 8 bits, codes 11000000 - 11000111
        assert_eq!(codes[280].1, 8);
        assert_eq!(codes[287].1, 8);
    }

    #[test]
    fn test_encode_literals() {
        let mut encoder = HuffmanEncoder::new(true);
        let tokens = vec![LZ77Token::Literal(b'H'), LZ77Token::Literal(b'i')];
        let data = encoder.encode(&tokens, true).unwrap();
        assert!(!data.is_empty());
    }
}
