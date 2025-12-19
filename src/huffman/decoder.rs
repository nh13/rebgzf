use crate::bits::BitReader;
use crate::error::{Error, Result};
use std::io::Read;

/// Canonical Huffman decoder
pub struct HuffmanDecoder {
    /// For each symbol, its code length (0 = not used)
    #[allow(dead_code)]
    code_lengths: Vec<u8>,
    /// Minimum code length
    #[allow(dead_code)]
    min_bits: u8,
    /// Maximum code length
    max_bits: u8,
    /// For each bit length, the starting code and starting index
    /// (first_code, first_symbol_index)
    bit_info: Vec<(u32, usize)>,
    /// Symbols sorted by code length, then by symbol value
    symbols: Vec<u16>,
}

impl HuffmanDecoder {
    /// Build from code lengths (for dynamic Huffman blocks)
    pub fn from_code_lengths(lengths: &[u8]) -> Result<Self> {
        if lengths.is_empty() {
            return Err(Error::HuffmanIncomplete);
        }

        let max_bits = *lengths.iter().max().unwrap_or(&0);
        if max_bits > 15 {
            return Err(Error::InvalidCodeLength(max_bits));
        }

        if max_bits == 0 {
            // All zero-length codes = empty table
            return Ok(Self {
                code_lengths: lengths.to_vec(),
                min_bits: 0,
                max_bits: 0,
                bit_info: vec![(0, 0); 16],
                symbols: vec![],
            });
        }

        // Count codes of each length
        let mut bl_count = [0u32; 16];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Find min_bits
        let min_bits = (1..=15).find(|&i| bl_count[i] > 0).unwrap_or(1) as u8;

        // Compute first code for each bit length
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..=max_bits {
            code = (code + bl_count[bits as usize - 1]) << 1;
            next_code[bits as usize] = code;
        }

        // Sort symbols by code length, then by symbol value
        let mut symbols: Vec<(u16, u8)> = lengths
            .iter()
            .enumerate()
            .filter(|(_, &len)| len > 0)
            .map(|(sym, &len)| (sym as u16, len))
            .collect();
        symbols.sort_by_key(|&(sym, len)| (len, sym));

        let sorted_symbols: Vec<u16> = symbols.iter().map(|&(sym, _)| sym).collect();

        // Build bit_info: for each bit length, store (first_code, first_symbol_index)
        let mut bit_info = vec![(0u32, 0usize); 16];
        let mut symbol_idx = 0;
        for bits in 1..=15 {
            bit_info[bits] = (next_code[bits], symbol_idx);
            symbol_idx += bl_count[bits] as usize;
        }

        Ok(Self {
            code_lengths: lengths.to_vec(),
            min_bits,
            max_bits,
            bit_info,
            symbols: sorted_symbols,
        })
    }

    /// Build fixed Huffman table for literal/length codes (RFC 1951 section 3.2.6)
    pub fn fixed_literal_length() -> Self {
        let lengths = super::tables::fixed_literal_lengths();
        Self::from_code_lengths(&lengths).unwrap()
    }

    /// Build fixed Huffman table for distance codes
    pub fn fixed_distance() -> Self {
        let lengths = super::tables::fixed_distance_lengths();
        Self::from_code_lengths(&lengths).unwrap()
    }

    /// Decode next symbol from bitstream
    pub fn decode<R: Read>(&self, bits: &mut BitReader<R>) -> Result<u16> {
        if self.max_bits == 0 {
            return Err(Error::HuffmanIncomplete);
        }

        let mut code = 0u32;
        for len in 1..=self.max_bits {
            code = (code << 1) | bits.read_bits(1)?;
            let (first_code, first_idx) = self.bit_info[len as usize];

            // Check if this code is valid for this length
            let count = if len < 15 {
                self.bit_info[len as usize + 1].1 - first_idx
            } else {
                self.symbols.len() - first_idx
            };

            if count > 0 && code >= first_code && code < first_code + count as u32 {
                let idx = first_idx + (code - first_code) as usize;
                return Ok(self.symbols[idx]);
            }
        }

        Err(Error::InvalidHuffmanSymbol(code as u16))
    }

    /// Check if this decoder is empty (no symbols)
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_fixed_literal_length() {
        let decoder = HuffmanDecoder::fixed_literal_length();
        assert!(!decoder.is_empty());
        assert_eq!(decoder.min_bits, 7);
        assert_eq!(decoder.max_bits, 9);
    }

    #[test]
    fn test_fixed_distance() {
        let decoder = HuffmanDecoder::fixed_distance();
        assert!(!decoder.is_empty());
        assert_eq!(decoder.min_bits, 5);
        assert_eq!(decoder.max_bits, 5);
    }

    #[test]
    fn test_simple_decode() {
        // Simple 2-symbol table: symbol 0 = code 0 (1 bit), symbol 1 = code 1 (1 bit)
        let lengths = vec![1, 1];
        let decoder = HuffmanDecoder::from_code_lengths(&lengths).unwrap();

        // Decode 0 -> symbol 0
        let data = vec![0b00000000];
        let mut reader = BitReader::new(Cursor::new(data));
        assert_eq!(decoder.decode(&mut reader).unwrap(), 0);

        // Decode 1 -> symbol 1
        let data = vec![0b00000001];
        let mut reader = BitReader::new(Cursor::new(data));
        assert_eq!(decoder.decode(&mut reader).unwrap(), 1);
    }
}
