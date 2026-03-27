//! Decoded DEFLATE chunk that may contain unresolved marker values.
//!
//! During parallel speculative decoding, chunks are decoded without knowing the
//! preceding 32KB window.  Back-references into the unknown window are stored as
//! [`MarkerValue`] entries.  Once the window becomes available, markers are
//! resolved in bulk via [`ChunkData::resolve_markers`].

use super::marker::{apply_window, contains_markers, MarkerValue};

/// Maximum DEFLATE sliding window size in bytes (32KB).
const WINDOW_SIZE: usize = 32_768;

/// A decoded DEFLATE chunk whose output may contain unresolved marker values.
///
/// Resolved byte buffers and marker buffers are stored separately.  After
/// [`resolve_markers`](Self::resolve_markers) is called with the correct window,
/// all marker buffers are converted to resolved byte buffers and the chunk
/// becomes fully resolved.
#[derive(Debug)]
pub struct ChunkData {
    /// Compressed offset in bits where this chunk starts.
    pub encoded_offset: usize,
    /// Compressed size in bits.
    pub encoded_size: usize,
    /// Fully resolved byte buffers.
    resolved: Vec<Vec<u8>>,
    /// Marker buffers awaiting window resolution.
    markers: Vec<Vec<MarkerValue>>,
    /// The 32KB window at the end of this chunk, used to resolve the next chunk.
    pub final_window: Option<Vec<u8>>,
    /// CRC32 of the decompressed data.
    pub crc32: Option<u32>,
}

impl ChunkData {
    /// Create a new chunk starting at the given compressed bit offset.
    pub fn new(encoded_offset: usize) -> Self {
        Self {
            encoded_offset,
            encoded_size: 0,
            resolved: Vec::new(),
            markers: Vec::new(),
            final_window: None,
            crc32: None,
        }
    }

    /// Append a fully resolved byte buffer to this chunk.
    pub fn append_resolved(&mut self, data: Vec<u8>) {
        if !data.is_empty() {
            self.resolved.push(data);
        }
    }

    /// Append a marker buffer awaiting window resolution.
    pub fn append_markers(&mut self, data: Vec<MarkerValue>) {
        if !data.is_empty() {
            self.markers.push(data);
        }
    }

    /// Returns `true` if all data in this chunk is resolved (no markers remain).
    pub fn is_resolved(&self) -> bool {
        self.markers.is_empty()
    }

    /// Total decompressed size in bytes (resolved bytes + marker count).
    pub fn decompressed_size(&self) -> usize {
        let resolved_bytes: usize = self.resolved.iter().map(|v| v.len()).sum();
        let marker_bytes: usize = self.markers.iter().map(|v| v.len()).sum();
        resolved_bytes + marker_bytes
    }

    /// Resolve all marker buffers using the provided window.
    ///
    /// Each marker buffer is resolved via [`apply_window`] and converted into a
    /// resolved byte buffer.  After this call, [`is_resolved`](Self::is_resolved)
    /// returns `true`.
    pub fn resolve_markers(&mut self, window: &[u8]) {
        for marker_buf in self.markers.drain(..) {
            let mut output = vec![0u8; marker_buf.len()];
            apply_window(&marker_buf, window, &mut output);
            self.resolved.push(output);
        }
    }

    /// Move the resolved data out of this chunk, leaving it empty.
    ///
    /// # Panics
    ///
    /// Panics if the chunk still contains unresolved markers.
    pub fn take_output(&mut self) -> Vec<Vec<u8>> {
        assert!(self.is_resolved(), "cannot take output: chunk has unresolved markers");
        std::mem::take(&mut self.resolved)
    }

    /// Borrow the resolved byte buffers.
    pub fn resolved_data(&self) -> &[Vec<u8>] {
        &self.resolved
    }

    /// Recompute the final window from the resolved data.
    ///
    /// This must be called after [`resolve_markers`](Self::resolve_markers) to ensure
    /// `final_window` contains correct bytes rather than placeholder zeros that were
    /// emitted for unresolved markers during speculative decoding.
    ///
    /// # Panics
    ///
    /// Panics if the chunk still contains unresolved markers.
    pub fn recompute_final_window(&mut self) {
        assert!(self.is_resolved(), "cannot recompute final window: chunk has unresolved markers");

        let total_bytes: usize = self.resolved.iter().map(|v| v.len()).sum();
        if total_bytes == 0 {
            self.final_window = Some(Vec::new());
            return;
        }

        // Always produce a full WINDOW_SIZE window, zero-padded at the start
        // if the chunk produced fewer bytes. Marker offsets are 0-based into a
        // 32KB window, so the window must always be exactly WINDOW_SIZE bytes.
        let mut result = vec![0u8; WINDOW_SIZE];

        if total_bytes >= WINDOW_SIZE {
            // Take last WINDOW_SIZE bytes from the resolved buffers.
            let mut skip = total_bytes - WINDOW_SIZE;
            let mut pos = 0;
            for buf in &self.resolved {
                if skip >= buf.len() {
                    skip -= buf.len();
                    continue;
                }
                let src = &buf[skip..];
                result[pos..pos + src.len()].copy_from_slice(src);
                pos += src.len();
                skip = 0;
            }
        } else {
            // Fewer than WINDOW_SIZE bytes: zero-pad at the start, data at the end.
            let pad = WINDOW_SIZE - total_bytes;
            let mut pos = pad;
            for buf in &self.resolved {
                result[pos..pos + buf.len()].copy_from_slice(buf);
                pos += buf.len();
            }
        }

        self.final_window = Some(result);
    }

    /// Returns `true` if any marker buffer contains actual window references
    /// (not just literals).
    pub fn has_window_references(&self) -> bool {
        self.markers.iter().any(|buf| contains_markers(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_chunk() {
        let chunk = ChunkData::new(1024);
        assert_eq!(chunk.encoded_offset, 1024);
        assert_eq!(chunk.encoded_size, 0);
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), 0);
        assert!(chunk.final_window.is_none());
        assert!(chunk.crc32.is_none());
        assert!(!chunk.has_window_references());
    }

    #[test]
    fn test_append_and_size() {
        let mut chunk = ChunkData::new(0);

        chunk.append_resolved(vec![1, 2, 3]);
        assert_eq!(chunk.decompressed_size(), 3);
        assert!(chunk.is_resolved());

        chunk.append_markers(vec![MarkerValue::literal(10), MarkerValue::window_ref(0)]);
        assert_eq!(chunk.decompressed_size(), 5);
        assert!(!chunk.is_resolved());

        // Empty appends should not change anything.
        chunk.append_resolved(vec![]);
        chunk.append_markers(vec![]);
        assert_eq!(chunk.decompressed_size(), 5);
    }

    #[test]
    fn test_resolve_markers() {
        let mut chunk = ChunkData::new(0);

        let window: Vec<u8> = vec![0xAA, 0xBB, 0xCC];

        chunk.append_markers(vec![
            MarkerValue::literal(0xFF),
            MarkerValue::window_ref(1),
            MarkerValue::window_ref(2),
        ]);
        assert!(!chunk.is_resolved());

        chunk.resolve_markers(&window);
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), 3);
        assert_eq!(chunk.resolved_data(), &[vec![0xFF, 0xBB, 0xCC]]);
    }

    #[test]
    fn test_take_output() {
        let mut chunk = ChunkData::new(0);
        let window: Vec<u8> = vec![0x42];

        chunk.append_resolved(vec![1, 2]);
        chunk.append_markers(vec![MarkerValue::window_ref(0)]);

        chunk.resolve_markers(&window);
        let output = chunk.take_output();
        assert_eq!(output, vec![vec![1, 2], vec![0x42]]);

        // After take, chunk is empty.
        assert_eq!(chunk.decompressed_size(), 0);
        assert!(chunk.is_resolved());
    }

    #[test]
    #[should_panic(expected = "unresolved markers")]
    fn test_take_output_panics_with_unresolved() {
        let mut chunk = ChunkData::new(0);
        chunk.append_markers(vec![MarkerValue::window_ref(0)]);
        let _ = chunk.take_output();
    }

    #[test]
    fn test_empty_chunk() {
        let mut chunk = ChunkData::new(0);
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), 0);
        assert!(!chunk.has_window_references());

        let output = chunk.take_output();
        assert!(output.is_empty());
    }

    #[test]
    fn test_resolved_data_accessor() {
        let mut chunk = ChunkData::new(0);
        chunk.append_resolved(vec![10, 20, 30]);
        chunk.append_resolved(vec![40, 50]);
        assert_eq!(chunk.resolved_data(), &[vec![10, 20, 30], vec![40, 50]]);
    }

    #[test]
    fn test_has_window_references() {
        let mut chunk = ChunkData::new(0);

        // All-literal markers: no window references.
        chunk.append_markers(vec![MarkerValue::literal(1), MarkerValue::literal(2)]);
        assert!(!chunk.has_window_references());

        // Add markers with an actual window ref.
        chunk.append_markers(vec![MarkerValue::window_ref(0)]);
        assert!(chunk.has_window_references());
    }
}
