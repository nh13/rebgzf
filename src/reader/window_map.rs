//! Thread-safe storage for sliding windows at chunk boundaries.
//!
//! As chunks complete decoding, their final window (last 32KB of output)
//! is stored here so that the next chunk can use it for marker resolution.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Thread-safe ordered map of 32KB windows keyed by encoded bit offset.
///
/// As chunks complete decoding, their final window (last 32KB of output)
/// is stored here. The next chunk uses this window for marker resolution.
pub struct WindowMap {
    inner: Mutex<BTreeMap<usize, Vec<u8>>>,
}

impl WindowMap {
    /// Creates a new empty `WindowMap`.
    pub fn new() -> Self {
        Self { inner: Mutex::new(BTreeMap::new()) }
    }

    /// Stores a window at the given encoded bit offset.
    pub fn insert(&self, encoded_offset: usize, window: Vec<u8>) {
        self.inner.lock().expect("WindowMap lock poisoned").insert(encoded_offset, window);
    }

    /// Retrieves a clone of the window at the given encoded bit offset, if present.
    pub fn get(&self, encoded_offset: usize) -> Option<Vec<u8>> {
        self.inner.lock().expect("WindowMap lock poisoned").get(&encoded_offset).cloned()
    }

    /// Removes and returns the window at the given encoded bit offset, if present.
    pub fn remove(&self, encoded_offset: usize) -> Option<Vec<u8>> {
        self.inner.lock().expect("WindowMap lock poisoned").remove(&encoded_offset)
    }

    /// Removes all windows with offsets strictly less than `offset`.
    ///
    /// This is used for memory management: once all chunks before a given offset
    /// have been fully resolved, their windows are no longer needed.
    pub fn release_before(&self, offset: usize) {
        let mut map = self.inner.lock().expect("WindowMap lock poisoned");
        // Split off entries >= offset, keeping only those.
        *map = map.split_off(&offset);
    }
}

impl Default for WindowMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_insert_and_get() {
        let map = WindowMap::new();
        let window = vec![0xAB; 32_768];
        map.insert(100, window.clone());

        let retrieved = map.get(100);
        assert_eq!(retrieved, Some(window));
    }

    #[test]
    fn test_get_missing() {
        let map = WindowMap::new();
        assert_eq!(map.get(42), None);
    }

    #[test]
    fn test_remove() {
        let map = WindowMap::new();
        let window = vec![0xCD; 1024];
        map.insert(200, window.clone());

        let removed = map.remove(200);
        assert_eq!(removed, Some(window));
        // After removal, get should return None.
        assert_eq!(map.get(200), None);
    }

    #[test]
    fn test_release_before() {
        let map = WindowMap::new();
        map.insert(10, vec![1]);
        map.insert(20, vec![2]);
        map.insert(30, vec![3]);
        map.insert(40, vec![4]);

        map.release_before(25);

        // Offsets 10 and 20 should be gone.
        assert_eq!(map.get(10), None);
        assert_eq!(map.get(20), None);
        // Offsets 30 and 40 should remain.
        assert_eq!(map.get(30), Some(vec![3]));
        assert_eq!(map.get(40), Some(vec![4]));
    }

    #[test]
    fn test_concurrent_access() {
        let map = Arc::new(WindowMap::new());
        let num_threads = 8;
        let entries_per_thread = 100;

        // Spawn threads that each insert a range of entries.
        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..entries_per_thread {
                        let offset = t * entries_per_thread + i;
                        map.insert(offset, vec![t as u8; 64]);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // Verify all entries are present with correct values.
        for t in 0..num_threads {
            for i in 0..entries_per_thread {
                let offset = t * entries_per_thread + i;
                let window = map.get(offset).expect("missing entry");
                assert_eq!(window, vec![t as u8; 64]);
            }
        }
    }
}
