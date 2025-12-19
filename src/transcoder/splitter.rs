use crate::deflate::tokens::LZ77Token;

/// Trait for determining optimal BGZF block split points.
///
/// Smart splitters can improve compression by aligning block boundaries
/// with logical record boundaries (e.g., FASTQ records, BAM records).
pub trait BlockSplitter {
    /// Process a token and update internal state.
    /// Called for each token as it's added to a pending block.
    fn process_token(&mut self, token: &LZ77Token);

    /// Check if the current position (after all processed tokens) is a good split point.
    /// Returns true if this would be a good place to end a BGZF block.
    fn is_good_split_point(&self) -> bool;

    /// Get the number of bytes since the last good split point.
    /// Used to determine if we should backtrack to a better boundary.
    fn bytes_since_last_good_split(&self) -> usize;

    /// Reset state for a new block.
    fn reset(&mut self);
}

/// Default splitter that considers every position a good split point.
/// This preserves the original simple size-based splitting behavior.
#[derive(Default)]
pub struct DefaultSplitter;

impl BlockSplitter for DefaultSplitter {
    fn process_token(&mut self, _token: &LZ77Token) {}

    fn is_good_split_point(&self) -> bool {
        true // Every position is acceptable
    }

    fn bytes_since_last_good_split(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

/// FASTQ-aware splitter that identifies record boundaries.
///
/// FASTQ records consist of 4 lines:
/// 1. @header (starts with @)
/// 2. sequence
/// 3. + (quality header, optional repeat of header)
/// 4. quality scores
///
/// This splitter tracks newlines and considers positions after
/// every 4th newline (end of quality line) as good split points.
pub struct FastqSplitter {
    /// Count of newlines seen in current block (mod 4)
    newline_count: u8,
    /// Bytes processed since last record boundary
    bytes_since_record_end: usize,
    /// Whether we're at a record boundary (after quality line)
    at_record_boundary: bool,
}

impl FastqSplitter {
    pub fn new() -> Self {
        Self {
            newline_count: 0,
            bytes_since_record_end: 0,
            at_record_boundary: true, // Start of file is a valid boundary
        }
    }
}

impl Default for FastqSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockSplitter for FastqSplitter {
    fn process_token(&mut self, token: &LZ77Token) {
        match token {
            LZ77Token::Literal(byte) => {
                self.bytes_since_record_end += 1;
                if *byte == b'\n' {
                    self.newline_count = (self.newline_count + 1) % 4;
                    if self.newline_count == 0 {
                        // Just finished a complete record
                        self.at_record_boundary = true;
                        self.bytes_since_record_end = 0;
                    } else {
                        self.at_record_boundary = false;
                    }
                } else {
                    self.at_record_boundary = false;
                }
            }
            LZ77Token::Copy { length, .. } => {
                // For copies, we need to track newlines in the copied data.
                // This is approximate - we don't have the actual bytes here.
                // We'll be conservative and assume we're not at a boundary.
                self.bytes_since_record_end += *length as usize;
                self.at_record_boundary = false;
            }
            LZ77Token::EndOfBlock => {}
        }
    }

    fn is_good_split_point(&self) -> bool {
        self.at_record_boundary
    }

    fn bytes_since_last_good_split(&self) -> usize {
        self.bytes_since_record_end
    }

    fn reset(&mut self) {
        // Don't reset newline_count - record boundaries span blocks
        self.bytes_since_record_end = 0;
        // Keep at_record_boundary state from previous block
    }
}

/// FASTQ-aware splitter that uses the uncompressed data from boundary resolution.
///
/// This is more accurate than FastqSplitter because it sees the actual
/// uncompressed bytes after Copy tokens are resolved.
pub struct FastqByteSplitter {
    /// Count of newlines seen (mod 4)
    newline_count: u8,
    /// Bytes processed since last record boundary
    bytes_since_record_end: usize,
    /// Whether we're at a record boundary
    at_record_boundary: bool,
}

impl FastqByteSplitter {
    pub fn new() -> Self {
        Self { newline_count: 0, bytes_since_record_end: 0, at_record_boundary: true }
    }

    /// Process raw bytes (called with uncompressed data)
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.bytes_since_record_end += 1;
            if byte == b'\n' {
                self.newline_count = (self.newline_count + 1) % 4;
                if self.newline_count == 0 {
                    self.at_record_boundary = true;
                    self.bytes_since_record_end = 0;
                } else {
                    self.at_record_boundary = false;
                }
            } else {
                self.at_record_boundary = false;
            }
        }
    }

    /// Check if at a good split point
    pub fn is_good_split_point(&self) -> bool {
        self.at_record_boundary
    }

    /// Bytes since last good split
    pub fn bytes_since_last_good_split(&self) -> usize {
        self.bytes_since_record_end
    }
}

impl Default for FastqByteSplitter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_splitter() {
        let splitter = DefaultSplitter;
        assert!(splitter.is_good_split_point());
        assert_eq!(splitter.bytes_since_last_good_split(), 0);
    }

    #[test]
    fn test_fastq_splitter_record_boundary() {
        let mut splitter = FastqSplitter::new();

        // Simulate a complete FASTQ record:
        // @header\nACGT\n+\nIIII\n

        // @header
        for &b in b"@header" {
            splitter.process_token(&LZ77Token::Literal(b));
        }
        splitter.process_token(&LZ77Token::Literal(b'\n'));
        assert!(!splitter.is_good_split_point()); // Line 1 done

        // ACGT
        for &b in b"ACGT" {
            splitter.process_token(&LZ77Token::Literal(b));
        }
        splitter.process_token(&LZ77Token::Literal(b'\n'));
        assert!(!splitter.is_good_split_point()); // Line 2 done

        // +
        splitter.process_token(&LZ77Token::Literal(b'+'));
        splitter.process_token(&LZ77Token::Literal(b'\n'));
        assert!(!splitter.is_good_split_point()); // Line 3 done

        // IIII (quality)
        for &b in b"IIII" {
            splitter.process_token(&LZ77Token::Literal(b));
        }
        splitter.process_token(&LZ77Token::Literal(b'\n'));
        assert!(splitter.is_good_split_point()); // Line 4 done - record boundary!
        assert_eq!(splitter.bytes_since_last_good_split(), 0);
    }

    #[test]
    fn test_fastq_byte_splitter() {
        let mut splitter = FastqByteSplitter::new();

        // Process a complete FASTQ record
        splitter.process_bytes(b"@header\nACGT\n+\nIIII\n");

        assert!(splitter.is_good_split_point());
        assert_eq!(splitter.bytes_since_last_good_split(), 0);

        // Process partial record
        splitter.process_bytes(b"@next\nAA");

        assert!(!splitter.is_good_split_point());
        assert!(splitter.bytes_since_last_good_split() > 0);
    }
}
