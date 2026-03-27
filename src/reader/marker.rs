/// Maximum DEFLATE sliding window size in bytes.
const MAX_WINDOW_SIZE: u16 = 32_768;

/// A 16-bit value encoding either a literal byte or a reference into the preceding window.
///
/// The encoding scheme partitions the `u16` range as follows:
/// - `0..=255`: literal byte value (the byte itself)
/// - `256..=32767`: reserved/invalid
/// - `32768..=65535`: window reference at offset `value - 32768`
///
/// During speculative DEFLATE decoding, back-references that fall into the unknown
/// preceding window are stored as marker values.  Once the true window becomes
/// available the markers are resolved to concrete bytes via [`MarkerValue::resolve`]
/// or in bulk via [`apply_window`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerValue(u16);

impl MarkerValue {
    /// Create a marker representing a literal byte value.
    #[inline]
    pub fn literal(byte: u8) -> Self {
        Self(byte as u16)
    }

    /// Create a marker representing a reference into the preceding window.
    ///
    /// `offset` must be in `0..MAX_WINDOW_SIZE` (i.e. `0..32768`).
    ///
    /// # Panics
    ///
    /// Panics if `offset >= MAX_WINDOW_SIZE`.
    #[inline]
    pub fn window_ref(offset: u16) -> Self {
        assert!(offset < MAX_WINDOW_SIZE, "window offset out of range: {offset}");
        Self(offset + MAX_WINDOW_SIZE)
    }

    /// Returns `true` if this value encodes a literal byte.
    #[inline]
    pub fn is_literal(self) -> bool {
        self.0 <= 255
    }

    /// Returns `true` if this value encodes a window reference.
    #[inline]
    pub fn is_marker(self) -> bool {
        self.0 >= MAX_WINDOW_SIZE
    }

    /// Resolve this value to a concrete byte.
    ///
    /// Literals return the byte directly.  Window references index into the
    /// provided `window` slice.
    ///
    /// # Panics
    ///
    /// Panics if this is a window reference and the offset is out of bounds for `window`,
    /// or if the value is in the reserved range (`256..32768`).
    #[inline]
    pub fn resolve(self, window: &[u8]) -> u8 {
        if self.is_literal() {
            self.0 as u8
        } else if self.is_marker() {
            let offset = (self.0 - MAX_WINDOW_SIZE) as usize;
            window[offset]
        } else {
            panic!("MarkerValue in reserved range: {}", self.0);
        }
    }
}

/// Resolve a slice of marker values into concrete bytes.
///
/// Each element of `markers` is resolved against `window` and written to the
/// corresponding position in `output`.
///
/// # Panics
///
/// Panics if `output.len() < markers.len()`, or if any marker cannot be resolved
/// (see [`MarkerValue::resolve`]).
#[inline]
pub fn apply_window(markers: &[MarkerValue], window: &[u8], output: &mut [u8]) {
    assert!(output.len() >= markers.len(), "output buffer too small for marker resolution");
    for (i, &marker) in markers.iter().enumerate() {
        output[i] = marker.resolve(window);
    }
}

/// Returns `true` if any element in `markers` is a window reference.
///
/// A slice consisting entirely of literals returns `false`, meaning no window
/// resolution is needed.
#[inline]
pub fn contains_markers(markers: &[MarkerValue]) -> bool {
    markers.iter().any(|m| m.is_marker())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_literal_roundtrip() {
        for byte in 0..=255u8 {
            let mv = MarkerValue::literal(byte);
            assert!(mv.is_literal(), "byte {byte} should be literal");
            assert!(!mv.is_marker(), "byte {byte} should not be marker");
            assert_eq!(mv.resolve(&[]), byte, "literal {byte} should resolve to itself");
        }
    }

    #[test]
    fn test_window_ref() {
        let window: Vec<u8> = (0..=255).cycle().take(MAX_WINDOW_SIZE as usize).collect();

        // Test offset 0
        let mv = MarkerValue::window_ref(0);
        assert!(!mv.is_literal());
        assert!(mv.is_marker());
        assert_eq!(mv.resolve(&window), window[0]);

        // Test offset 1
        let mv = MarkerValue::window_ref(1);
        assert_eq!(mv.resolve(&window), window[1]);

        // Test maximum valid offset
        let max_offset = MAX_WINDOW_SIZE - 1;
        let mv = MarkerValue::window_ref(max_offset);
        assert!(mv.is_marker());
        assert_eq!(mv.resolve(&window), window[max_offset as usize]);

        // Test a mid-range offset
        let mv = MarkerValue::window_ref(1000);
        assert_eq!(mv.resolve(&window), window[1000]);
    }

    #[test]
    fn test_apply_window() {
        let window: Vec<u8> = vec![10, 20, 30, 40, 50];
        let markers = vec![
            MarkerValue::literal(0xFF),
            MarkerValue::window_ref(2),
            MarkerValue::literal(0x42),
            MarkerValue::window_ref(0),
            MarkerValue::window_ref(4),
        ];
        let mut output = vec![0u8; markers.len()];
        apply_window(&markers, &window, &mut output);
        assert_eq!(output, vec![0xFF, 30, 0x42, 10, 50]);
    }

    #[test]
    fn test_contains_markers() {
        // All literals — no markers
        let all_literals =
            vec![MarkerValue::literal(0), MarkerValue::literal(128), MarkerValue::literal(255)];
        assert!(!contains_markers(&all_literals));

        // Mixed — contains markers
        let mixed = vec![MarkerValue::literal(0), MarkerValue::window_ref(0)];
        assert!(contains_markers(&mixed));

        // All markers
        let all_markers = vec![MarkerValue::window_ref(0), MarkerValue::window_ref(100)];
        assert!(contains_markers(&all_markers));

        // Empty slice
        assert!(!contains_markers(&[]));
    }

    #[test]
    fn test_edge_offsets() {
        let window = vec![0xAA; MAX_WINDOW_SIZE as usize];

        // Offset 0 (minimum)
        let mv = MarkerValue::window_ref(0);
        assert_eq!(mv.resolve(&window), 0xAA);

        // Offset MAX_WINDOW_SIZE - 1 (maximum valid)
        let mv = MarkerValue::window_ref(MAX_WINDOW_SIZE - 1);
        assert_eq!(mv.resolve(&window), 0xAA);
    }

    #[test]
    #[should_panic(expected = "reserved range")]
    fn test_reserved_range_panics() {
        // Construct a value in the reserved range (256..32768) manually
        let mv = MarkerValue(256);
        mv.resolve(&[]);
    }
}
