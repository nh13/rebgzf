use crate::error::Result;

/// Trait for bit-level reading from a data source.
///
/// Implementations provide the core bit operations needed by DEFLATE parsing
/// and Huffman decoding. This trait allows the hot path to be generic over
/// the backing store (Read-based stream vs memory-mapped slice).
pub trait BitRead {
    /// Ensure at least `n` bits are available in the buffer.
    fn fill_buffer(&mut self, n: u8) -> Result<()>;

    /// Read `n` bits (1-32) in LSB-first order (standard DEFLATE order).
    fn read_bits(&mut self, n: u8) -> Result<u32>;

    /// Peek at `n` bits without consuming them (for table-based Huffman decoding).
    fn peek_bits(&mut self, n: u8) -> Result<u32>;

    /// Consume `n` bits that were previously peeked.
    fn consume_bits(&mut self, n: u8);

    /// Read a single bit.
    #[inline]
    fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Discard remaining bits in current byte, align to next byte boundary.
    fn align_to_byte(&mut self);

    /// Read a complete byte (aligns to byte boundary first).
    #[inline]
    fn read_byte(&mut self) -> Result<u8> {
        self.align_to_byte();
        self.read_bits(8).map(|v| v as u8)
    }

    /// Read a 16-bit little-endian value (aligns to byte boundary first).
    #[inline]
    fn read_u16_le(&mut self) -> Result<u16> {
        self.align_to_byte();
        let lo = self.read_bits(8)? as u16;
        let hi = self.read_bits(8)? as u16;
        Ok(lo | (hi << 8))
    }

    /// Read a 32-bit little-endian value (aligns to byte boundary first).
    #[inline]
    fn read_u32_le(&mut self) -> Result<u32> {
        self.align_to_byte();
        let b0 = self.read_bits(8)?;
        let b1 = self.read_bits(8)?;
        let b2 = self.read_bits(8)?;
        let b3 = self.read_bits(8)?;
        Ok(b0 | (b1 << 8) | (b2 << 16) | (b3 << 24))
    }

    /// Read exactly `n` bytes into a buffer (aligns to byte boundary first).
    #[inline]
    fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        self.align_to_byte();
        for b in buf.iter_mut() {
            *b = self.read_bits(8)? as u8;
        }
        Ok(())
    }

    /// Get approximate position in bytes (for error reporting).
    fn bytes_read(&self) -> u64;
}
