use super::tables::{CODE_LENGTH_ORDER, DISTANCE_TABLE, LENGTH_TABLE};
use super::tokens::{CodeLengths, LZ77Block, LZ77Token};
use crate::bits::BitReader;
use crate::error::{Error, Result};
use crate::huffman::HuffmanDecoder;
use std::io::Read;

/// Parses DEFLATE blocks and extracts LZ77 stream
pub struct DeflateParser<R: Read> {
    bits: BitReader<R>,
    /// Whether we've seen the final block
    finished: bool,
}

impl<R: Read> DeflateParser<R> {
    pub fn new(reader: R) -> Self {
        Self { bits: BitReader::new(reader), finished: false }
    }

    /// Parse the next DEFLATE block, returning LZ77 tokens
    /// Returns None when stream is exhausted
    pub fn parse_block(&mut self) -> Result<Option<LZ77Block>> {
        if self.finished {
            return Ok(None);
        }

        let is_final = self.bits.read_bit()?;
        let block_type = self.bits.read_bits(2)? as u8;

        let block = match block_type {
            0 => self.parse_stored_block(is_final)?,
            1 => self.parse_fixed_block(is_final)?,
            2 => self.parse_dynamic_block(is_final)?,
            _ => return Err(Error::InvalidBlockType(block_type)),
        };

        if is_final {
            self.finished = true;
        }

        Ok(Some(block))
    }

    /// Parse a stored (uncompressed) block
    fn parse_stored_block(&mut self, is_final: bool) -> Result<LZ77Block> {
        // Align to byte boundary
        self.bits.align_to_byte();

        // Read LEN and NLEN
        let len = self.bits.read_u16_le()?;
        let nlen = self.bits.read_u16_le()?;

        // Verify LEN and NLEN are complements
        if len != !nlen {
            return Err(Error::StoredBlockLengthMismatch { len, nlen });
        }

        // Read literal bytes
        let mut tokens = Vec::with_capacity(len as usize + 1);
        for _ in 0..len {
            let byte = self.bits.read_bits(8)? as u8;
            tokens.push(LZ77Token::Literal(byte));
        }
        tokens.push(LZ77Token::EndOfBlock);

        Ok(LZ77Block::new(tokens, is_final, 0))
    }

    /// Parse a block with fixed Huffman codes
    fn parse_fixed_block(&mut self, is_final: bool) -> Result<LZ77Block> {
        let lit_decoder = HuffmanDecoder::fixed_literal_length();
        let dist_decoder = HuffmanDecoder::fixed_distance();

        let tokens = self.decode_symbols(&lit_decoder, &dist_decoder)?;
        Ok(LZ77Block::new(tokens, is_final, 1))
    }

    /// Parse a block with dynamic Huffman codes
    fn parse_dynamic_block(&mut self, is_final: bool) -> Result<LZ77Block> {
        // Read header
        let hlit = self.bits.read_bits(5)? as usize + 257; // # of literal/length codes
        let hdist = self.bits.read_bits(5)? as usize + 1; // # of distance codes
        let hclen = self.bits.read_bits(4)? as usize + 4; // # of code length codes

        // Read code length code lengths
        let mut code_length_lengths = [0u8; 19];
        for i in 0..hclen {
            code_length_lengths[CODE_LENGTH_ORDER[i]] = self.bits.read_bits(3)? as u8;
        }

        // Build code length decoder
        let code_length_decoder = HuffmanDecoder::from_code_lengths(&code_length_lengths)?;

        // Decode literal/length and distance code lengths
        let total_codes = hlit + hdist;
        let mut all_lengths = Vec::with_capacity(total_codes);

        while all_lengths.len() < total_codes {
            let sym = code_length_decoder.decode(&mut self.bits)?;

            match sym {
                0..=15 => {
                    // Literal code length
                    all_lengths.push(sym as u8);
                }
                16 => {
                    // Copy previous code length 3-6 times
                    let repeat = self.bits.read_bits(2)? as usize + 3;
                    let prev = *all_lengths.last().ok_or(Error::HuffmanIncomplete)?;
                    for _ in 0..repeat {
                        all_lengths.push(prev);
                    }
                }
                17 => {
                    // Repeat zero 3-10 times
                    let repeat = self.bits.read_bits(3)? as usize + 3;
                    all_lengths.resize(all_lengths.len() + repeat, 0);
                }
                18 => {
                    // Repeat zero 11-138 times
                    let repeat = self.bits.read_bits(7)? as usize + 11;
                    all_lengths.resize(all_lengths.len() + repeat, 0);
                }
                _ => return Err(Error::InvalidHuffmanSymbol(sym)),
            }
        }

        // Split into literal/length and distance lengths
        let literal_lengths: Vec<u8> = all_lengths[..hlit].to_vec();
        let distance_lengths: Vec<u8> = all_lengths[hlit..].to_vec();

        // Build decoders
        let lit_decoder = HuffmanDecoder::from_code_lengths(&literal_lengths)?;
        let dist_decoder = if distance_lengths.iter().all(|&l| l == 0) {
            // No distance codes - this is valid for blocks with only literals
            None
        } else {
            Some(HuffmanDecoder::from_code_lengths(&distance_lengths)?)
        };

        // Decode symbols
        let tokens = self.decode_symbols_with_optional_dist(&lit_decoder, dist_decoder.as_ref())?;

        let mut block = LZ77Block::new(tokens, is_final, 2);
        block.code_lengths = Some(CodeLengths { literal_lengths, distance_lengths });

        Ok(block)
    }

    /// Decode symbols using literal/length and distance decoders
    fn decode_symbols(
        &mut self,
        lit_decoder: &HuffmanDecoder,
        dist_decoder: &HuffmanDecoder,
    ) -> Result<Vec<LZ77Token>> {
        self.decode_symbols_with_optional_dist(lit_decoder, Some(dist_decoder))
    }

    /// Decode symbols, optionally using a distance decoder
    fn decode_symbols_with_optional_dist(
        &mut self,
        lit_decoder: &HuffmanDecoder,
        dist_decoder: Option<&HuffmanDecoder>,
    ) -> Result<Vec<LZ77Token>> {
        let mut tokens = Vec::with_capacity(1024);

        loop {
            let sym = lit_decoder.decode(&mut self.bits)?;

            match sym {
                0..=255 => {
                    // Literal byte
                    tokens.push(LZ77Token::Literal(sym as u8));
                }
                256 => {
                    // End of block
                    tokens.push(LZ77Token::EndOfBlock);
                    break;
                }
                257..=285 => {
                    // Length code
                    let len_idx = (sym - 257) as usize;
                    let (base_len, extra_bits) = LENGTH_TABLE[len_idx];
                    let extra = if extra_bits > 0 { self.bits.read_bits(extra_bits)? } else { 0 };
                    let length = base_len + extra as u16;

                    // Read distance
                    let dist_decoder = dist_decoder.ok_or(Error::InvalidDistanceCode(0))?;
                    let dist_sym = dist_decoder.decode(&mut self.bits)?;
                    if dist_sym > 29 {
                        return Err(Error::InvalidDistanceCode(dist_sym));
                    }

                    let (base_dist, dist_extra_bits) = DISTANCE_TABLE[dist_sym as usize];
                    let dist_extra = if dist_extra_bits > 0 {
                        self.bits.read_bits(dist_extra_bits)?
                    } else {
                        0
                    };
                    let distance = base_dist + dist_extra as u16;

                    tokens.push(LZ77Token::Copy { length, distance });
                }
                _ => {
                    return Err(Error::InvalidLengthCode(sym));
                }
            }
        }

        Ok(tokens)
    }

    /// Get bytes read so far
    pub fn bytes_read(&self) -> u64 {
        self.bits.bytes_read()
    }

    /// Check if we've finished parsing
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Get the underlying bit reader (for reading trailer)
    pub fn into_inner(self) -> BitReader<R> {
        self.bits
    }

    /// Read the gzip trailer (CRC32, ISIZE) and check for another gzip member.
    /// Returns Ok(true) if another member follows, Ok(false) if EOF.
    /// Must be called after all DEFLATE blocks are parsed (is_finished() == true).
    pub fn read_trailer_and_check_next(&mut self) -> Result<bool> {
        if !self.finished {
            return Err(Error::Internal("Cannot read trailer before DEFLATE is finished".into()));
        }

        // Align to byte boundary (discard any remaining bits)
        self.bits.align_to_byte();

        // Read CRC32 and ISIZE (we don't validate them, just skip)
        let _crc32 = self.bits.read_u32_le()?;
        let _isize = self.bits.read_u32_le()?;

        // Try to read the next gzip magic bytes
        match self.bits.read_bits(8) {
            Ok(b1) => {
                match self.bits.read_bits(8) {
                    Ok(b2) => {
                        if b1 == 0x1f && b2 == 0x8b {
                            // Another gzip member! Reset parser state.
                            // Skip the rest of the gzip header (we already read magic)
                            // Read compression method
                            let method = self.bits.read_bits(8)? as u8;
                            if method != 8 {
                                return Err(Error::UnsupportedCompressionMethod(method));
                            }

                            // Read flags
                            let flags = self.bits.read_bits(8)? as u8;

                            // Skip mtime (4 bytes), xfl (1), os (1)
                            let _mtime = self.bits.read_u32_le()?;
                            let _xfl = self.bits.read_bits(8)?;
                            let _os = self.bits.read_bits(8)?;

                            // Handle optional fields based on flags
                            const FEXTRA: u8 = 1 << 2;
                            const FNAME: u8 = 1 << 3;
                            const FCOMMENT: u8 = 1 << 4;
                            const FHCRC: u8 = 1 << 1;

                            if flags & FEXTRA != 0 {
                                let xlen = self.bits.read_u16_le()?;
                                for _ in 0..xlen {
                                    self.bits.read_bits(8)?;
                                }
                            }

                            if flags & FNAME != 0 {
                                // Read null-terminated string
                                loop {
                                    if self.bits.read_bits(8)? == 0 {
                                        break;
                                    }
                                }
                            }

                            if flags & FCOMMENT != 0 {
                                // Read null-terminated string
                                loop {
                                    if self.bits.read_bits(8)? == 0 {
                                        break;
                                    }
                                }
                            }

                            if flags & FHCRC != 0 {
                                let _hcrc = self.bits.read_u16_le()?;
                            }

                            // Reset finished flag for next member
                            self.finished = false;
                            Ok(true)
                        } else {
                            // Not a gzip header - probably garbage or wrong format
                            Err(Error::InvalidGzipMagic(((b2 as u16) << 8) | (b1 as u16)))
                        }
                    }
                    Err(Error::UnexpectedEof) => Ok(false), // EOF after first byte
                    Err(e) => Err(e),
                }
            }
            Err(Error::UnexpectedEof) => Ok(false), // Clean EOF
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_stored_block() {
        // Stored block: BFINAL=1, BTYPE=00, LEN=5, NLEN=!5, "Hello"
        let data = vec![
            0b00000001, // BFINAL=1, BTYPE=00 (stored) - packed LSB first
            0x05, 0x00, // LEN = 5
            0xFA, 0xFF, // NLEN = !5 = 0xFFFA
            b'H', b'e', b'l', b'l', b'o',
        ];

        let mut parser = DeflateParser::new(Cursor::new(data));
        let block = parser.parse_block().unwrap().unwrap();

        assert!(block.is_final);
        assert_eq!(block.block_type, 0);
        assert_eq!(block.tokens.len(), 6); // 5 literals + EndOfBlock

        // Verify content
        assert_eq!(block.tokens[0], LZ77Token::Literal(b'H'));
        assert_eq!(block.tokens[1], LZ77Token::Literal(b'e'));
        assert_eq!(block.tokens[2], LZ77Token::Literal(b'l'));
        assert_eq!(block.tokens[3], LZ77Token::Literal(b'l'));
        assert_eq!(block.tokens[4], LZ77Token::Literal(b'o'));
        assert_eq!(block.tokens[5], LZ77Token::EndOfBlock);
    }

    #[test]
    fn test_parse_real_gzip() {
        // Use flate2 to create a test deflate stream
        use std::io::Write;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"Hello, World!").unwrap();
        let compressed = encoder.finish().unwrap();

        let mut parser = DeflateParser::new(Cursor::new(compressed));
        let mut total_size = 0;

        while let Some(block) = parser.parse_block().unwrap() {
            total_size += block.uncompressed_size();
            if block.is_final {
                break;
            }
        }

        assert_eq!(total_size, 13);
    }
}
