//! Marker replacement for speculative decode chunks.

use super::chunk::ChunkData;

/// Resolve all markers in a chunk and optionally compute CRC32.
///
/// Called when the previous chunk completes and its final window is available.
/// After this call, `chunk.is_resolved()` returns true.
///
/// If `compute_crc` is true, also computes and stores the CRC32 of the
/// resolved data in `chunk.crc32`.
pub fn replace_markers(chunk: &mut ChunkData, window: &[u8], compute_crc: bool) {
    chunk.resolve_markers(window);
    chunk.recompute_final_window();

    if compute_crc {
        let mut hasher = crc32fast::Hasher::new();
        for buf in chunk.resolved_data() {
            hasher.update(buf);
        }
        chunk.crc32 = Some(hasher.finalize());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::marker::MarkerValue;

    #[test]
    fn test_replace_markers_basic() {
        let mut chunk = ChunkData::new(0);
        // Mix of resolved data and marker data referencing the window.
        chunk.append_resolved(vec![0x01, 0x02]);
        chunk.append_markers(vec![
            MarkerValue::literal(0xFF),
            MarkerValue::window_ref(0),
            MarkerValue::window_ref(2),
        ]);
        assert!(!chunk.is_resolved());

        let window = vec![0xAA, 0xBB, 0xCC];
        replace_markers(&mut chunk, &window, false);

        assert!(chunk.is_resolved());
        assert!(chunk.crc32.is_none());

        // Verify resolved data: original resolved buf + resolved markers buf.
        let data = chunk.resolved_data();
        assert_eq!(data[0], vec![0x01, 0x02]);
        assert_eq!(data[1], vec![0xFF, 0xAA, 0xCC]);

        // final_window should be recomputed — always 32KB, zero-padded at the front.
        assert!(chunk.final_window.is_some());
        let fw = chunk.final_window.as_ref().unwrap();
        let expected_data = [0x01u8, 0x02, 0xFF, 0xAA, 0xCC];
        assert_eq!(fw.len(), 32768);
        assert_eq!(&fw[32768 - expected_data.len()..], &expected_data);
    }

    #[test]
    fn test_replace_markers_with_crc() {
        let mut chunk = ChunkData::new(0);
        chunk.append_markers(vec![MarkerValue::window_ref(0), MarkerValue::literal(0x42)]);

        let window = vec![0x10, 0x20, 0x30];
        replace_markers(&mut chunk, &window, true);

        assert!(chunk.is_resolved());
        assert!(chunk.crc32.is_some());

        // Compute the expected CRC32 independently.
        let resolved_bytes = vec![0x10u8, 0x42];
        let expected_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(&resolved_bytes);
            h.finalize()
        };
        assert_eq!(chunk.crc32.unwrap(), expected_crc);
    }

    #[test]
    fn test_replace_no_markers() {
        let mut chunk = ChunkData::new(0);
        chunk.append_resolved(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        // Already resolved — replace_markers should be a no-op on the data.
        assert!(chunk.is_resolved());
        let window = vec![];
        replace_markers(&mut chunk, &window, false);

        assert!(chunk.is_resolved());
        assert!(chunk.crc32.is_none());
        assert_eq!(chunk.resolved_data(), &[vec![0xDE, 0xAD, 0xBE, 0xEF]]);

        // final_window should still be recomputed — always 32KB, zero-padded at the front.
        let fw = chunk.final_window.as_ref().unwrap();
        let expected_data = [0xDEu8, 0xAD, 0xBE, 0xEF];
        assert_eq!(fw.len(), 32768);
        assert_eq!(&fw[32768 - expected_data.len()..], &expected_data);
    }
}
