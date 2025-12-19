use crate::error::{Error, Result};
use std::io::Read;

/// Bit-level reader for DEFLATE streams
///
/// DEFLATE uses LSB-first bit ordering within bytes.
/// Bits are read from LSB to MSB within each byte.
pub struct BitReader<R: Read> {
    reader: R,
    /// Buffer holding up to 64 bits
    buffer: u64,
    /// Number of valid bits in buffer (0-64)
    bits_available: u8,
    /// Total bytes read (for error reporting)
    bytes_read: u64,
}

impl<R: Read> BitReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, buffer: 0, bits_available: 0, bytes_read: 0 }
    }

    /// Ensure at least `n` bits are available in buffer
    ///
    /// Uses bulk refill: reads up to 8 bytes at once when buffer is low,
    /// reducing syscall overhead significantly for bit-level operations.
    fn fill_buffer(&mut self, n: u8) -> Result<()> {
        debug_assert!(n <= 57, "Cannot request more than 57 bits at once");

        // Fast path: already have enough bits
        if self.bits_available >= n {
            return Ok(());
        }

        // Bulk refill: read up to 8 bytes at once when buffer has room
        // We can safely add bytes when bits_available <= 56 (room for 8 bits minimum)
        if self.bits_available <= 56 {
            let bytes_to_read = ((64 - self.bits_available) / 8) as usize;
            let mut bulk_buf = [0u8; 8];

            match self.reader.read(&mut bulk_buf[..bytes_to_read]) {
                Ok(0) => {
                    // No bytes available - fall through to byte-by-byte for EOF handling
                }
                Ok(bytes_read) => {
                    // Add all read bytes to buffer at once
                    for &byte in &bulk_buf[..bytes_read] {
                        self.buffer |= (byte as u64) << self.bits_available;
                        self.bits_available += 8;
                    }
                    self.bytes_read += bytes_read as u64;

                    // If we now have enough, we're done
                    if self.bits_available >= n {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    // Retry on interrupt
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }

        // Fallback: byte-by-byte for remaining needs or EOF detection
        while self.bits_available < n {
            let mut byte = [0u8; 1];
            match self.reader.read_exact(&mut byte) {
                Ok(()) => {
                    self.buffer |= (byte[0] as u64) << self.bits_available;
                    self.bits_available += 8;
                    self.bytes_read += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Err(Error::UnexpectedEof);
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(())
    }

    /// Read `n` bits (1-32) in LSB-first order (standard DEFLATE order)
    pub fn read_bits(&mut self, n: u8) -> Result<u32> {
        debug_assert!(n <= 32, "Cannot read more than 32 bits at once");

        if n == 0 {
            return Ok(0);
        }

        self.fill_buffer(n)?;

        let mask = (1u64 << n) - 1;
        let result = (self.buffer & mask) as u32;
        self.buffer >>= n;
        self.bits_available -= n;

        Ok(result)
    }

    /// Peek at `n` bits without consuming them (for table-based Huffman decoding)
    #[inline]
    pub fn peek_bits(&mut self, n: u8) -> Result<u32> {
        debug_assert!(n <= 32, "Cannot peek more than 32 bits at once");

        if n == 0 {
            return Ok(0);
        }

        self.fill_buffer(n)?;

        let mask = (1u64 << n) - 1;
        Ok((self.buffer & mask) as u32)
    }

    /// Consume `n` bits that were previously peeked
    #[inline]
    pub fn consume_bits(&mut self, n: u8) {
        debug_assert!(n <= self.bits_available, "Cannot consume more bits than available");
        self.buffer >>= n;
        self.bits_available -= n;
    }

    /// Read a single bit
    #[inline]
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Discard remaining bits in current byte, align to next byte boundary
    pub fn align_to_byte(&mut self) {
        let discard = self.bits_available % 8;
        if discard > 0 {
            self.buffer >>= discard;
            self.bits_available -= discard;
        }
    }

    /// Read a complete byte (aligns to byte boundary first)
    pub fn read_byte(&mut self) -> Result<u8> {
        self.align_to_byte();
        self.read_bits(8).map(|v| v as u8)
    }

    /// Read a 16-bit little-endian value (aligns to byte boundary first)
    pub fn read_u16_le(&mut self) -> Result<u16> {
        self.align_to_byte();
        let lo = self.read_bits(8)? as u16;
        let hi = self.read_bits(8)? as u16;
        Ok(lo | (hi << 8))
    }

    /// Read a 32-bit little-endian value (aligns to byte boundary first)
    pub fn read_u32_le(&mut self) -> Result<u32> {
        self.align_to_byte();
        let b0 = self.read_bits(8)?;
        let b1 = self.read_bits(8)?;
        let b2 = self.read_bits(8)?;
        let b3 = self.read_bits(8)?;
        Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
    }

    /// Read exactly `n` bytes into a buffer (aligns to byte boundary first)
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        self.align_to_byte();
        for b in buf.iter_mut() {
            *b = self.read_bits(8)? as u8;
        }
        Ok(())
    }

    /// Get position in bytes (approximate, for error reporting)
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Check if we have bits available without reading more
    pub fn bits_available(&self) -> u8 {
        self.bits_available
    }

    /// Get the inner reader (consumes self)
    pub fn into_inner(self) -> R {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bits() {
        // Binary: 11010011 10101010 = 0xD3 0xAA
        let data = vec![0xD3, 0xAA];
        let mut reader = BitReader::new(data.as_slice());

        // Read LSB first: 0xD3 = 11010011, reading from LSB:
        // bits 0-2: 011 = 3
        assert_eq!(reader.read_bits(3).unwrap(), 0b011);
        // bits 3-7: 11010 = 26
        assert_eq!(reader.read_bits(5).unwrap(), 0b11010);
        // next byte
        assert_eq!(reader.read_bits(8).unwrap(), 0xAA);
    }

    #[test]
    fn test_read_bit() {
        let data = vec![0b10110001];
        let mut reader = BitReader::new(data.as_slice());

        // LSB first
        assert!(reader.read_bit().unwrap()); // 1
        assert!(!reader.read_bit().unwrap()); // 0
        assert!(!reader.read_bit().unwrap()); // 0
        assert!(!reader.read_bit().unwrap()); // 0
        assert!(reader.read_bit().unwrap()); // 1
        assert!(reader.read_bit().unwrap()); // 1
        assert!(!reader.read_bit().unwrap()); // 0
        assert!(reader.read_bit().unwrap()); // 1
    }

    #[test]
    fn test_align_to_byte() {
        let data = vec![0xFF, 0xAB];
        let mut reader = BitReader::new(data.as_slice());

        reader.read_bits(3).unwrap();
        reader.align_to_byte();
        assert_eq!(reader.read_bits(8).unwrap(), 0xAB);
    }

    #[test]
    fn test_read_u16_le() {
        let data = vec![0x34, 0x12]; // Little-endian 0x1234
        let mut reader = BitReader::new(data.as_slice());
        assert_eq!(reader.read_u16_le().unwrap(), 0x1234);
    }

    #[test]
    fn test_read_u32_le() {
        let data = vec![0x78, 0x56, 0x34, 0x12]; // Little-endian 0x12345678
        let mut reader = BitReader::new(data.as_slice());
        assert_eq!(reader.read_u32_le().unwrap(), 0x12345678);
    }

    #[test]
    fn test_cross_byte_boundary() {
        let data = vec![0xFF, 0x00];
        let mut reader = BitReader::new(data.as_slice());

        // Read 12 bits across byte boundary
        assert_eq!(reader.read_bits(12).unwrap(), 0x0FF);
    }
}
