/// 32KB circular buffer for LZ77 sliding window
pub struct SlidingWindow {
    buffer: Box<[u8; 32768]>,
    /// Next write position (0-32767)
    write_pos: usize,
    /// Total bytes ever written
    total_written: u64,
}

impl SlidingWindow {
    pub fn new() -> Self {
        Self { buffer: Box::new([0u8; 32768]), write_pos: 0, total_written: 0 }
    }

    /// Add a single byte to the window
    #[inline(always)]
    pub fn push_byte(&mut self, byte: u8) {
        // Safety: write_pos is always masked to 0x7FFF, so always < 32768
        unsafe { *self.buffer.get_unchecked_mut(self.write_pos) = byte };
        self.write_pos = (self.write_pos + 1) & 0x7FFF;
        self.total_written += 1;
    }

    /// Add multiple bytes to the window efficiently using bulk copy.
    ///
    /// Handles inputs of any length, including those larger than the 32KB window.
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let remaining = 32768 - self.write_pos;
            let chunk_len = remaining.min(bytes.len() - offset);
            self.buffer[self.write_pos..self.write_pos + chunk_len]
                .copy_from_slice(&bytes[offset..offset + chunk_len]);
            self.write_pos = (self.write_pos + chunk_len) & 0x7FFF;
            offset += chunk_len;
        }
        self.total_written += bytes.len() as u64;
    }

    /// Get `length` bytes from `distance` bytes back
    ///
    /// Note: distance=1 means the most recently written byte.
    /// Length can exceed distance (run-length encoding case).
    pub fn get(&self, distance: u16, length: u16) -> Vec<u8> {
        let mut result = Vec::with_capacity(length as usize);
        self.copy_to_vec(distance, length, &mut result);
        result
    }

    /// Copy `length` bytes from `distance` bytes back into a pre-allocated Vec.
    /// This avoids allocation when the caller can reuse a buffer.
    #[inline]
    pub fn copy_to_vec(&self, distance: u16, length: u16, out: &mut Vec<u8>) {
        debug_assert!((1..=32768).contains(&distance));

        let dist = distance as usize;
        let len = length as usize;
        let available = self.total_written.min(32768) as usize;
        debug_assert!(
            dist <= available,
            "invalid back-reference: distance {dist} exceeds available window {available}"
        );
        let start = (self.write_pos + 32768 - dist) & 0x7FFF;

        if dist >= len {
            // Fast path: non-RLE, source doesn't overlap destination
            out.reserve(len);
            let end = start + len;
            if end <= 32768 {
                out.extend_from_slice(&self.buffer[start..end]);
            } else {
                // Wraps around circular buffer
                out.extend_from_slice(&self.buffer[start..]);
                out.extend_from_slice(&self.buffer[..end & 0x7FFF]);
            }
        } else {
            // RLE case: distance < length, must handle overlap byte-by-byte
            let start_len = out.len();
            let mut read_pos = start;
            for i in 0..len {
                if i < dist {
                    // Safety: read_pos is always masked to 0x7FFF, so always < 32768
                    out.push(unsafe { *self.buffer.get_unchecked(read_pos) });
                    read_pos = (read_pos + 1) & 0x7FFF;
                } else {
                    // RLE: copy from earlier in output
                    let rle_idx = start_len + i - dist;
                    out.push(unsafe { *out.get_unchecked(rle_idx) });
                }
            }
        }
    }

    /// Process each byte from `distance` bytes back, calling the provided closure.
    /// This avoids allocation entirely for cases where we just need to iterate.
    #[inline]
    pub fn for_each_byte<F: FnMut(u8)>(&self, distance: u16, length: u16, mut f: F) {
        debug_assert!((1..=32768).contains(&distance));

        let available = self.total_written.min(32768) as usize;
        debug_assert!(
            (distance as usize) <= available,
            "invalid back-reference: distance {} exceeds available window {available}",
            distance
        );
        let start = (self.write_pos + 32768 - distance as usize) & 0x7FFF;

        if length <= distance {
            // Simple case: no RLE, just read from buffer
            let mut read_pos = start;
            for _ in 0..length {
                f(self.buffer[read_pos]);
                read_pos = (read_pos + 1) & 0x7FFF;
            }
        } else {
            // RLE case: need to track what we've "produced"
            // We need a small buffer for the pattern
            let dist = distance as usize;
            let mut pattern = Vec::with_capacity(dist);

            // First, get the pattern bytes
            let mut read_pos = start;
            for _ in 0..dist {
                pattern.push(self.buffer[read_pos]);
                read_pos = (read_pos + 1) & 0x7FFF;
            }

            // Now emit the pattern repeatedly
            for i in 0..length as usize {
                f(pattern[i % dist]);
            }
        }
    }

    /// Get available window size
    pub fn available(&self) -> usize {
        self.total_written.min(32768) as usize
    }

    /// Get total bytes written
    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    /// Reset the window
    pub fn clear(&mut self) {
        self.write_pos = 0;
        self.total_written = 0;
    }
}

impl Default for SlidingWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_basic() {
        let mut window = SlidingWindow::new();
        window.push_byte(b'A');
        window.push_byte(b'B');
        window.push_byte(b'C');

        assert_eq!(window.get(1, 1), vec![b'C']);
        assert_eq!(window.get(2, 1), vec![b'B']);
        assert_eq!(window.get(3, 1), vec![b'A']);
        assert_eq!(window.get(3, 3), vec![b'A', b'B', b'C']);
    }

    #[test]
    fn test_window_rle() {
        let mut window = SlidingWindow::new();
        window.push_byte(b'A');

        // RLE case: distance=1, length=5 -> "AAAAA"
        assert_eq!(window.get(1, 5), vec![b'A', b'A', b'A', b'A', b'A']);
    }

    #[test]
    fn test_window_rle_pattern() {
        let mut window = SlidingWindow::new();
        window.push_byte(b'A');
        window.push_byte(b'B');

        // RLE case: distance=2, length=6 -> "ABABAB"
        assert_eq!(window.get(2, 6), vec![b'A', b'B', b'A', b'B', b'A', b'B']);
    }

    #[test]
    fn test_push_bytes_bulk() {
        let mut window = SlidingWindow::new();
        window.push_bytes(b"ABCDEFGH");
        assert_eq!(window.get(8, 8), b"ABCDEFGH");
        assert_eq!(window.total_written(), 8);
    }

    #[test]
    fn test_push_bytes_wrapping() {
        let mut window = SlidingWindow::new();
        // Fill to near end
        for i in 0..32766u32 {
            window.push_byte((i & 0xFF) as u8);
        }
        // Push 4 bytes that wrap around
        window.push_bytes(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(window.get(4, 4), vec![0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(window.total_written(), 32770);
    }

    #[test]
    fn test_copy_to_vec_non_rle_no_wrap() {
        let mut window = SlidingWindow::new();
        window.push_bytes(b"ABCDEFGH");
        let mut out = Vec::new();
        window.copy_to_vec(8, 4, &mut out);
        assert_eq!(out, b"ABCD");
    }

    #[test]
    fn test_copy_to_vec_non_rle_wrapping() {
        let mut window = SlidingWindow::new();
        for i in 0..32766u32 {
            window.push_byte((i & 0xFF) as u8);
        }
        window.push_bytes(&[0xAA, 0xBB]);
        let mut out = Vec::new();
        window.copy_to_vec(2, 2, &mut out);
        assert_eq!(out, vec![0xAA, 0xBB]);
    }

    #[test]
    fn test_push_bytes_larger_than_window() {
        let mut window = SlidingWindow::new();
        // Push more than 32KB in one call — should not panic
        let data: Vec<u8> = (0..40000u32).map(|i| (i & 0xFF) as u8).collect();
        window.push_bytes(&data);
        assert_eq!(window.total_written(), 40000);
        assert_eq!(window.available(), 32768);
        // Most recent byte should be the last in data
        assert_eq!(window.get(1, 1), vec![(39999 & 0xFF) as u8]);
    }

    #[test]
    fn test_window_wrap() {
        let mut window = SlidingWindow::new();

        // Fill buffer past 32KB
        for i in 0..40000u32 {
            window.push_byte((i & 0xFF) as u8);
        }

        assert_eq!(window.available(), 32768);
        assert_eq!(window.total_written(), 40000);

        // Most recent byte should be (39999 & 0xFF) = 63
        assert_eq!(window.get(1, 1), vec![63]);
    }
}
