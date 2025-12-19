/// Bit-level writer for DEFLATE output
///
/// Writes bits LSB-first to match DEFLATE format.
/// Uses a 64-bit buffer for bulk writes, flushing when full.
pub struct BitWriter {
    /// Accumulated output bytes
    output: Vec<u8>,
    /// 64-bit buffer for accumulating bits before flushing
    buffer: u64,
    /// Number of bits currently in buffer (0-64)
    bits_in_buffer: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { output: Vec::with_capacity(65536), buffer: 0, bits_in_buffer: 0 }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self { output: Vec::with_capacity(capacity), buffer: 0, bits_in_buffer: 0 }
    }

    /// Flush complete bytes from buffer to output
    #[inline]
    fn flush_bytes(&mut self) {
        while self.bits_in_buffer >= 8 {
            self.output.push(self.buffer as u8);
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }
    }

    /// Write `n` bits (1-32) from value in LSB-first order
    #[inline]
    pub fn write_bits(&mut self, value: u32, n: u8) {
        debug_assert!(n <= 32);

        if n == 0 {
            return;
        }

        // Add bits to buffer
        self.buffer |= (value as u64) << self.bits_in_buffer;
        self.bits_in_buffer += n;

        // Flush if buffer is getting full (leave room for next write)
        if self.bits_in_buffer >= 32 {
            self.flush_bytes();
        }
    }

    /// Write a single bit
    #[inline]
    pub fn write_bit(&mut self, bit: bool) {
        self.write_bits(bit as u32, 1);
    }

    /// Write bits in reversed order (for Huffman codes stored MSB-first)
    /// The code is `length` bits, with MSB first
    pub fn write_bits_reversed(&mut self, code: u32, length: u8) {
        let reversed = reverse_bits(code, length);
        self.write_bits(reversed, length);
    }

    /// Pad to byte boundary with zero bits
    pub fn align_to_byte(&mut self) {
        if self.bits_in_buffer % 8 != 0 {
            // Round up to next byte boundary
            self.bits_in_buffer = ((self.bits_in_buffer + 7) / 8) * 8;
        }
        self.flush_bytes();
    }

    /// Write a raw byte (must be byte-aligned)
    pub fn write_byte(&mut self, byte: u8) {
        if self.bits_in_buffer % 8 == 0 {
            // Byte-aligned: flush buffer first, then push directly
            self.flush_bytes();
            self.output.push(byte);
        } else {
            // Not aligned, write through bits
            self.write_bits(byte as u32, 8);
        }
    }

    /// Write a 16-bit value in little-endian
    pub fn write_u16_le(&mut self, value: u16) {
        self.write_byte(value as u8);
        self.write_byte((value >> 8) as u8);
    }

    /// Write a 32-bit value in little-endian
    pub fn write_u32_le(&mut self, value: u32) {
        self.write_byte(value as u8);
        self.write_byte((value >> 8) as u8);
        self.write_byte((value >> 16) as u8);
        self.write_byte((value >> 24) as u8);
    }

    /// Write raw bytes
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Finish and return the output bytes
    pub fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.output
    }

    /// Get current output length in bytes (including partial byte)
    pub fn len(&self) -> usize {
        self.output.len() + if self.bits_in_buffer > 0 { 1 } else { 0 }
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.output.is_empty() && self.bits_in_buffer == 0
    }

    /// Peek at output without consuming
    pub fn as_bytes(&self) -> &[u8] {
        &self.output
    }

    /// Clear the writer for reuse
    pub fn clear(&mut self) {
        self.output.clear();
        self.buffer = 0;
        self.bits_in_buffer = 0;
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Reverse the bottom `n` bits of `value`
fn reverse_bits(value: u32, n: u8) -> u32 {
    let mut result = 0u32;
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

    #[test]
    fn test_write_bits() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b011, 3); // bits 0-2
        writer.write_bits(0b11010, 5); // bits 3-7
        let output = writer.finish();
        assert_eq!(output, vec![0xD3]); // 11010_011 = 0xD3
    }

    #[test]
    fn test_write_cross_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xFFF, 12); // 12 bits: 1111_1111_1111
        let output = writer.finish();
        assert_eq!(output, vec![0xFF, 0x0F]);
    }

    #[test]
    fn test_write_u16_le() {
        let mut writer = BitWriter::new();
        writer.write_u16_le(0x1234);
        let output = writer.finish();
        assert_eq!(output, vec![0x34, 0x12]);
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(reverse_bits(0b10101, 5), 0b10101);
        assert_eq!(reverse_bits(0b11110000, 8), 0b00001111);
    }

    #[test]
    fn test_write_bits_reversed() {
        let mut writer = BitWriter::new();
        // Write 0b1100 (4 bits) reversed -> 0b0011
        writer.write_bits_reversed(0b1100, 4);
        writer.write_bits(0, 4); // Pad to byte
        let output = writer.finish();
        assert_eq!(output[0] & 0x0F, 0b0011);
    }
}
