use crate::bits::BitReader;
use crate::error::{Error, Result};
use std::io::Read;

/// Number of bits for the primary lookup table
/// 10 bits = 1024 entries, covers most DEFLATE codes efficiently
const LOOKUP_BITS: u8 = 10;
const LOOKUP_SIZE: usize = 1 << LOOKUP_BITS;

/// Entry in the lookup table
/// Packed format: low 11 bits = symbol (0-2047), high 5 bits = code length (1-15, 0 = invalid)
/// If code_length > LOOKUP_BITS, this entry is invalid and we need bit-by-bit decoding
#[derive(Clone, Copy, Default)]
struct LookupEntry(u16);

impl LookupEntry {
    const SYMBOL_MASK: u16 = 0x07FF; // 11 bits for symbol
    const LENGTH_SHIFT: u16 = 11;

    #[inline]
    fn new(symbol: u16, length: u8) -> Self {
        debug_assert!(symbol <= Self::SYMBOL_MASK);
        debug_assert!(length <= 15);
        Self(symbol | ((length as u16) << Self::LENGTH_SHIFT))
    }

    #[inline]
    fn symbol(self) -> u16 {
        self.0 & Self::SYMBOL_MASK
    }

    #[inline]
    fn length(self) -> u8 {
        (self.0 >> Self::LENGTH_SHIFT) as u8
    }

    #[inline]
    fn is_valid(self) -> bool {
        self.length() > 0 && self.length() <= LOOKUP_BITS
    }
}

/// Canonical Huffman decoder with table-based fast path
pub struct HuffmanDecoder {
    /// Primary lookup table for fast decoding of short codes
    lookup: Box<[LookupEntry; LOOKUP_SIZE]>,
    /// For each bit length, the starting code and starting index
    /// (first_code, first_symbol_index) - used for fallback
    bit_info: Vec<(u32, usize)>,
    /// Symbols sorted by code length, then by symbol value - used for fallback
    symbols: Vec<u16>,
    /// Maximum code length (for fallback path)
    max_bits: u8,
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
                lookup: Box::new([LookupEntry::default(); LOOKUP_SIZE]),
                bit_info: vec![(0, 0); 16],
                symbols: vec![],
                max_bits: 0,
            });
        }

        // Count codes of each length
        let mut bl_count = [0u32; 16];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Compute first code for each bit length
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..=max_bits {
            code = (code + bl_count[bits as usize - 1]) << 1;
            next_code[bits as usize] = code;
        }

        // Build lookup table and symbol list
        let mut lookup = Box::new([LookupEntry::default(); LOOKUP_SIZE]);
        let mut symbols_with_len: Vec<(u16, u8, u32)> = Vec::new(); // (symbol, length, code)

        // Assign codes to symbols and populate lookup table
        let mut current_code = next_code.clone();
        for (sym, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }

            let code = current_code[len as usize];
            current_code[len as usize] += 1;
            symbols_with_len.push((sym as u16, len, code));

            // If code fits in lookup table, populate entries
            if len <= LOOKUP_BITS {
                // Reverse bits for DEFLATE's bit ordering
                let reversed = reverse_bits(code, len);

                // Fill all entries where the low `len` bits match
                // The remaining high bits can be anything
                let fill_count = 1 << (LOOKUP_BITS - len);
                for suffix in 0..fill_count {
                    let idx = reversed as usize | (suffix << len);
                    lookup[idx] = LookupEntry::new(sym as u16, len);
                }
            }
        }

        // Sort symbols by (length, symbol) for fallback path
        symbols_with_len.sort_by_key(|&(sym, len, _)| (len, sym));
        let sorted_symbols: Vec<u16> = symbols_with_len.iter().map(|&(sym, _, _)| sym).collect();

        // Build bit_info for fallback
        let mut bit_info = vec![(0u32, 0usize); 16];
        let mut symbol_idx = 0;
        for bits in 1..=15 {
            bit_info[bits] = (next_code[bits], symbol_idx);
            symbol_idx += bl_count[bits] as usize;
        }

        Ok(Self {
            lookup,
            bit_info,
            symbols: sorted_symbols,
            max_bits,
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

    /// Decode next symbol from bitstream using table lookup with fallback
    #[inline]
    pub fn decode<R: Read>(&self, bits: &mut BitReader<R>) -> Result<u16> {
        if self.max_bits == 0 {
            return Err(Error::HuffmanIncomplete);
        }

        // Fast path: try to peek LOOKUP_BITS and do table lookup
        // If we can't peek enough bits (near EOF), fall back to slow path
        if let Ok(peek) = bits.peek_bits(LOOKUP_BITS) {
            let entry = self.lookup[peek as usize];

            if entry.is_valid() {
                // Found it! Consume exactly the code length bits
                bits.consume_bits(entry.length());
                return Ok(entry.symbol());
            }
        }

        // Slow path: bit-by-bit for codes longer than LOOKUP_BITS or near EOF
        self.decode_slow(bits)
    }

    /// Slow path for codes longer than LOOKUP_BITS
    #[cold]
    fn decode_slow<R: Read>(&self, bits: &mut BitReader<R>) -> Result<u16> {
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

/// Reverse `n` bits of a value (for DEFLATE's bit ordering in lookup table)
#[inline]
fn reverse_bits(value: u32, n: u8) -> u32 {
    let mut result = 0;
    let mut v = value;
    for _ in 0..n {
        result = (result << 1) | (v & 1);
        v >>= 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_fixed_literal_length() {
        let decoder = HuffmanDecoder::fixed_literal_length();
        assert!(!decoder.is_empty());
        assert_eq!(decoder.max_bits, 9);
    }

    #[test]
    fn test_fixed_distance() {
        let decoder = HuffmanDecoder::fixed_distance();
        assert!(!decoder.is_empty());
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

    #[test]
    fn test_lookup_entry() {
        let entry = LookupEntry::new(256, 8);
        assert_eq!(entry.symbol(), 256);
        assert_eq!(entry.length(), 8);
        assert!(entry.is_valid());

        let entry2 = LookupEntry::new(100, 12);
        assert_eq!(entry2.symbol(), 100);
        assert_eq!(entry2.length(), 12);
        assert!(!entry2.is_valid()); // > LOOKUP_BITS
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(reverse_bits(0b101, 3), 0b101);
        assert_eq!(reverse_bits(0b100, 3), 0b001);
        assert_eq!(reverse_bits(0b001, 3), 0b100);
        assert_eq!(reverse_bits(0b1100, 4), 0b0011);
    }
}
