use super::window::SlidingWindow;
use crate::deflate::tokens::LZ77Token;

/// Resolves LZ77 back-references that cross BGZF block boundaries.
///
/// The key insight: we only need to resolve references where the
/// referenced data would be in a *previous* BGZF block. References
/// within the same block can remain as Copy tokens.
pub struct BoundaryResolver {
    /// 32KB sliding window of resolved (uncompressed) bytes
    window: SlidingWindow,
    /// Current position in the uncompressed stream
    position: u64,
    /// Statistics
    refs_resolved: u64,
    refs_preserved: u64,
}

impl BoundaryResolver {
    pub fn new() -> Self {
        Self { window: SlidingWindow::new(), position: 0, refs_resolved: 0, refs_preserved: 0 }
    }

    /// Process tokens for a BGZF block.
    ///
    /// `block_start`: position where this BGZF block starts
    /// `tokens`: LZ77 tokens to process
    ///
    /// Returns: tokens with cross-boundary references resolved to literals
    pub fn resolve_block(&mut self, block_start: u64, tokens: &[LZ77Token]) -> Vec<LZ77Token> {
        let mut output = Vec::with_capacity(tokens.len());

        for token in tokens {
            match token {
                LZ77Token::Literal(byte) => {
                    // Literals pass through unchanged
                    self.window.push_byte(*byte);
                    self.position += 1;
                    output.push(LZ77Token::Literal(*byte));
                }

                LZ77Token::Copy { length, distance } => {
                    // Check if reference crosses block boundary
                    let ref_start = self.position.saturating_sub(*distance as u64);

                    if ref_start < block_start {
                        // Reference points to previous block - must resolve
                        let resolved = self.resolve_copy(*length, *distance);
                        for byte in &resolved {
                            self.window.push_byte(*byte);
                            output.push(LZ77Token::Literal(*byte));
                        }
                        self.position += *length as u64;
                        self.refs_resolved += 1;
                    } else {
                        // Reference stays within current block - preserve it
                        // But we still need to update the window!
                        let resolved = self.resolve_copy(*length, *distance);
                        for byte in &resolved {
                            self.window.push_byte(*byte);
                        }
                        self.position += *length as u64;
                        output.push(LZ77Token::Copy { length: *length, distance: *distance });
                        self.refs_preserved += 1;
                    }
                }

                LZ77Token::EndOfBlock => {
                    // Don't include EndOfBlock in output - we'll add our own
                }
            }
        }

        output
    }

    /// Resolve a Copy reference to literal bytes using the window
    fn resolve_copy(&self, length: u16, distance: u16) -> Vec<u8> {
        self.window.get(distance, length)
    }

    /// Get the current position in uncompressed stream
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Get statistics (resolved, preserved)
    pub fn stats(&self) -> (u64, u64) {
        (self.refs_resolved, self.refs_preserved)
    }

    /// Reset the resolver
    pub fn reset(&mut self) {
        self.window.clear();
        self.position = 0;
        self.refs_resolved = 0;
        self.refs_preserved = 0;
    }
}

impl Default for BoundaryResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literals_only() {
        let mut resolver = BoundaryResolver::new();

        let tokens = vec![LZ77Token::Literal(b'H'), LZ77Token::Literal(b'i')];
        let resolved = resolver.resolve_block(0, &tokens);

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], LZ77Token::Literal(b'H'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'i'));
        assert_eq!(resolver.position(), 2);
    }

    #[test]
    fn test_copy_within_block() {
        let mut resolver = BoundaryResolver::new();

        // Block starts at 0, contains "ABAB" where second AB is a copy
        let tokens = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Copy { length: 2, distance: 2 }, // Copy "AB"
        ];
        let resolved = resolver.resolve_block(0, &tokens);

        // Copy should be preserved since it references within block
        assert_eq!(resolved.len(), 3);
        assert!(matches!(resolved[2], LZ77Token::Copy { .. }));

        let (refs_resolved, refs_preserved) = resolver.stats();
        assert_eq!(refs_resolved, 0);
        assert_eq!(refs_preserved, 1);
    }

    #[test]
    fn test_copy_crosses_boundary() {
        let mut resolver = BoundaryResolver::new();

        // First block: "ABCD"
        let tokens1 = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Literal(b'C'),
            LZ77Token::Literal(b'D'),
        ];
        let _ = resolver.resolve_block(0, &tokens1);
        assert_eq!(resolver.position(), 4);

        // Second block starting at position 4
        // Contains a reference back to first block
        let tokens2 = vec![
            LZ77Token::Literal(b'E'),
            LZ77Token::Copy { length: 2, distance: 5 }, // refs "AB" in block 1
        ];
        let resolved = resolver.resolve_block(4, &tokens2);

        // Copy should be resolved to literals since it references previous block
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0], LZ77Token::Literal(b'E'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'A'));
        assert_eq!(resolved[2], LZ77Token::Literal(b'B'));

        let (refs_resolved, refs_preserved) = resolver.stats();
        assert_eq!(refs_resolved, 1);
        assert_eq!(refs_preserved, 0);
    }

    #[test]
    fn test_mixed_copies() {
        let mut resolver = BoundaryResolver::new();

        // First block: "ABCD"
        let tokens1 = vec![
            LZ77Token::Literal(b'A'),
            LZ77Token::Literal(b'B'),
            LZ77Token::Literal(b'C'),
            LZ77Token::Literal(b'D'),
        ];
        let _ = resolver.resolve_block(0, &tokens1);

        // Second block: "E" + copy from block 1 + copy within block 2
        let tokens2 = vec![
            LZ77Token::Literal(b'E'),
            LZ77Token::Copy { length: 2, distance: 5 }, // refs block 1 -> resolve
            LZ77Token::Copy { length: 2, distance: 1 }, // refs within block 2 -> preserve
        ];
        let resolved = resolver.resolve_block(4, &tokens2);

        // Should have: E, A, B, Copy(2,1)
        assert_eq!(resolved.len(), 4);
        assert_eq!(resolved[0], LZ77Token::Literal(b'E'));
        assert_eq!(resolved[1], LZ77Token::Literal(b'A'));
        assert_eq!(resolved[2], LZ77Token::Literal(b'B'));
        assert!(matches!(resolved[3], LZ77Token::Copy { length: 2, distance: 1 }));
    }
}
