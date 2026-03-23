use crate::bits::traits::BitRead;
use crate::error::{Error, Result};

/// High-performance bit reader operating on a byte slice (`&[u8]`).
///
/// Uses the Giesen "Variant 4" / dougallj zero-refill-latency technique:
/// - 64-bit buffer with branchless bulk refill via `ptr::read_unaligned`
/// - Refill is unconditional when buffer drops below 56 bits
/// - No syscalls, no error handling in the hot path
/// - The refill address is computed before the current decode completes,
///   enabling out-of-order execution to overlap refill with decode
///
/// References:
/// - Fabian Giesen: "Reading bits in far too many ways, part 2"
/// - dougallj: "Reading bits with zero refill latency"
pub struct SliceBitReader<'a> {
    /// Backing data (e.g. from mmap)
    data: &'a [u8],
    /// Current byte position in data
    pos: usize,
    /// 64-bit buffer holding pending bits (LSB-first)
    buffer: u64,
    /// Number of valid bits currently in buffer (0-64)
    bits_available: u8,
}

impl<'a> SliceBitReader<'a> {
    /// Create a new SliceBitReader from a byte slice.
    ///
    /// The slice should have at least 8 bytes of readable padding at the end
    /// for safe unaligned reads. If the slice is the exact file contents,
    /// the reader will fall back to safe byte-by-byte reads near the end.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, buffer: 0, bits_available: 0 }
    }

    /// Branchless bulk refill: load up to 7 bytes from the slice.
    ///
    /// This is the key optimization. We load bytes unconditionally when
    /// bits_available <= 56, which means the load address can be computed
    /// before the current decode result is known.
    #[inline(always)]
    fn refill(&mut self) {
        // Only refill if we have room for at least 1 byte
        if self.bits_available > 56 {
            return;
        }

        let bytes_can_consume = ((63 - self.bits_available) / 8) as usize;
        let bytes_remaining = self.data.len().saturating_sub(self.pos);
        let bytes_to_read = bytes_can_consume.min(bytes_remaining);

        if bytes_to_read == 0 {
            return;
        }

        // Fast path: at least 8 bytes remaining, use unaligned u64 read
        if bytes_remaining >= 8 {
            // Safety: we verified at least 8 bytes remain at self.pos
            let raw = unsafe { (self.data.as_ptr().add(self.pos) as *const u64).read_unaligned() };
            // On big-endian platforms we'd need to_le(); on little-endian (x86/ARM) this is a no-op
            let raw = u64::from_le(raw);
            // Mask to only the bytes we intend to consume. The u64 read may pull in up to
            // 8 bytes, but we only want `bytes_to_read` of them. Without masking, the extra
            // high bits would be shifted into the buffer and could corrupt a subsequent refill
            // (the next `|=` would OR into already-set high bits).
            let mask = if bytes_to_read < 8 { (1u64 << (bytes_to_read * 8)) - 1 } else { u64::MAX };
            self.buffer |= (raw & mask) << self.bits_available;
            self.pos += bytes_to_read;
            self.bits_available += (bytes_to_read * 8) as u8;
        } else {
            // Near end of data: byte-by-byte (rare, only last few bytes of file)
            for _ in 0..bytes_to_read {
                self.buffer |= (self.data[self.pos] as u64) << self.bits_available;
                self.pos += 1;
                self.bits_available += 8;
            }
        }
    }

    /// Get current byte position in the data.
    /// Note: this is the byte position from which the next refill will read.
    /// Bits already in the buffer have been consumed from earlier bytes.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Get the precise bit position: (byte_offset_of_next_unconsumed_bit, bit_within_that_byte).
    /// This accounts for bits already buffered but not yet consumed.
    pub fn bit_position(&self) -> (usize, u8) {
        let buffered_bytes = (self.bits_available / 8) as usize;
        let remaining_bits = self.bits_available % 8;
        let effective_byte = self.pos - buffered_bytes;
        // If there are remaining bits, we're partway through a byte
        if remaining_bits > 0 {
            (effective_byte - 1, 8 - remaining_bits)
        } else {
            (effective_byte, 0)
        }
    }

    /// Set position to a byte offset, resetting the bit buffer.
    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos;
        self.buffer = 0;
        self.bits_available = 0;
    }

    /// Set position to an arbitrary bit position within the data.
    /// `byte_pos` is the byte offset, `bit_offset` is 0-7 within that byte.
    pub fn set_bit_position(&mut self, byte_pos: usize, bit_offset: u8) {
        debug_assert!(bit_offset < 8);
        self.pos = byte_pos;
        self.buffer = 0;
        self.bits_available = 0;
        if bit_offset > 0 {
            // Load the byte and discard the low bits
            self.refill();
            if self.bits_available >= bit_offset {
                self.buffer >>= bit_offset;
                self.bits_available -= bit_offset;
            }
        }
    }

    /// Check if we've reached (or passed) a given byte position.
    /// Useful for parallel parsing: a thread stops when it reaches the next chunk's boundary.
    pub fn past_position(&self, byte_pos: usize) -> bool {
        // pos is where the next refill reads from.
        // Subtract buffered bytes (ceiling division) to get true consumed position.
        // Ceiling accounts for a partially consumed byte still being "buffered".
        let buffered_bytes = ((self.bits_available + 7) / 8) as usize;
        self.pos.saturating_sub(buffered_bytes) >= byte_pos
    }
}

impl<'a> BitRead for SliceBitReader<'a> {
    #[inline(always)]
    fn fill_buffer(&mut self, n: u8) -> Result<()> {
        if self.bits_available >= n {
            return Ok(());
        }
        self.refill();
        if self.bits_available >= n {
            Ok(())
        } else {
            Err(Error::UnexpectedEof)
        }
    }

    #[inline(always)]
    fn read_bits(&mut self, n: u8) -> Result<u32> {
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

    #[inline(always)]
    fn peek_bits(&mut self, n: u8) -> Result<u32> {
        debug_assert!(n <= 32, "Cannot peek more than 32 bits at once");

        if n == 0 {
            return Ok(0);
        }

        self.fill_buffer(n)?;

        let mask = (1u64 << n) - 1;
        Ok((self.buffer & mask) as u32)
    }

    #[inline(always)]
    fn consume_bits(&mut self, n: u8) {
        debug_assert!(n <= self.bits_available, "Cannot consume more bits than available");
        self.buffer >>= n;
        self.bits_available -= n;
    }

    #[inline]
    fn align_to_byte(&mut self) {
        let discard = self.bits_available % 8;
        if discard > 0 {
            self.buffer >>= discard;
            self.bits_available -= discard;
        }
    }

    fn bytes_read(&self) -> u64 {
        self.pos as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bits() {
        let data = vec![0xD3, 0xAA, 0, 0, 0, 0, 0, 0]; // padding for safety
        let mut reader = SliceBitReader::new(&data);

        // Read LSB first: 0xD3 = 11010011
        assert_eq!(reader.read_bits(3).unwrap(), 0b011);
        assert_eq!(reader.read_bits(5).unwrap(), 0b11010);
        assert_eq!(reader.read_bits(8).unwrap(), 0xAA);
    }

    #[test]
    fn test_read_bit() {
        let data = vec![0b10110001, 0, 0, 0, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);

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
    fn test_peek_consume() {
        let data = vec![0xFF, 0x00, 0, 0, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);

        let peeked = reader.peek_bits(10).unwrap();
        assert_eq!(peeked, 0x0FF); // 8 ones + 2 zeros
        reader.consume_bits(4);
        let next = reader.read_bits(8).unwrap();
        assert_eq!(next, 0x0F); // remaining 4 ones from first byte + 4 zeros from second
    }

    #[test]
    fn test_align_to_byte() {
        let data = vec![0xFF, 0xAB, 0, 0, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);

        reader.read_bits(3).unwrap();
        reader.align_to_byte();
        assert_eq!(reader.read_bits(8).unwrap(), 0xAB);
    }

    #[test]
    fn test_cross_byte_boundary() {
        let data = vec![0xFF, 0x00, 0, 0, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);
        assert_eq!(reader.read_bits(12).unwrap(), 0x0FF);
    }

    #[test]
    fn test_u16_le() {
        let data = vec![0x34, 0x12, 0, 0, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);
        assert_eq!(reader.read_u16_le().unwrap(), 0x1234);
    }

    #[test]
    fn test_u32_le() {
        let data = vec![0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0];
        let mut reader = SliceBitReader::new(&data);
        assert_eq!(reader.read_u32_le().unwrap(), 0x12345678);
    }

    #[test]
    fn test_past_position_partial_byte() {
        // After refilling 7 bytes and consuming 3 bits, we're partway through byte 0.
        // past_position(1) should be false since we haven't consumed all of byte 0 yet.
        let data = vec![0xFF; 16];
        let mut reader = SliceBitReader::new(&data);
        reader.read_bits(3).unwrap(); // consume 3 bits from byte 0
        let (byte, bit) = reader.bit_position();
        assert_eq!((byte, bit), (0, 3));
        // We're still in byte 0, so we shouldn't be past byte 1
        assert!(!reader.past_position(1));
    }

    #[test]
    fn test_past_position_full_bytes() {
        let data = vec![0xFF; 16];
        let mut reader = SliceBitReader::new(&data);
        reader.read_bits(16).unwrap(); // consume 2 full bytes
        assert!(reader.past_position(2));
        assert!(!reader.past_position(3));
    }
}
