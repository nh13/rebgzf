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
    #[inline]
    pub fn push_byte(&mut self, byte: u8) {
        self.buffer[self.write_pos] = byte;
        self.write_pos = (self.write_pos + 1) & 0x7FFF; // mod 32768
        self.total_written += 1;
    }

    /// Add multiple bytes to the window
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push_byte(b);
        }
    }

    /// Get `length` bytes from `distance` bytes back
    ///
    /// Note: distance=1 means the most recently written byte.
    /// Length can exceed distance (run-length encoding case).
    pub fn get(&self, distance: u16, length: u16) -> Vec<u8> {
        debug_assert!((1..=32768).contains(&distance));

        let mut result = Vec::with_capacity(length as usize);

        // Starting position in circular buffer
        // write_pos points to NEXT write location, so we go back (distance) from there
        let available = self.total_written.min(32768) as usize;
        let start = (self.write_pos + 32768 - (distance as usize).min(available)) & 0x7FFF;

        // Handle the RLE case: distance < length
        // We read byte-by-byte, handling wrap-around
        let mut read_pos = start;
        for i in 0..length as usize {
            if i < distance as usize {
                result.push(self.buffer[read_pos]);
                read_pos = (read_pos + 1) & 0x7FFF;
            } else {
                // RLE: copy from earlier in result
                let rle_idx = i - (distance as usize);
                result.push(result[rle_idx]);
            }
        }

        result
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
