//! GZI index file builder for BGZF random access.
//!
//! The GZI format stores offset pairs that map between compressed and
//! uncompressed positions in a BGZF file. This enables efficient random
//! access to any position in the uncompressed data.
//!
//! Format:
//! - Number of entries: u64 (little-endian)
//! - For each entry:
//!   - Compressed offset: u64 (little-endian)
//!   - Uncompressed offset: u64 (little-endian)

use std::io::{self, Write};

/// An entry in the GZI index mapping compressed to uncompressed offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GziEntry {
    /// Byte offset in the compressed BGZF file (start of block)
    pub compressed_offset: u64,
    /// Byte offset in the uncompressed data stream
    pub uncompressed_offset: u64,
}

/// Builder for GZI index files.
///
/// Tracks block offsets during transcoding and writes the index at the end.
#[derive(Debug, Default)]
pub struct GziIndexBuilder {
    entries: Vec<GziEntry>,
    current_compressed_offset: u64,
    current_uncompressed_offset: u64,
}

impl GziIndexBuilder {
    /// Create a new GZI index builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the start of a new BGZF block.
    ///
    /// Call this before writing each block to record its position.
    pub fn add_block(&mut self, compressed_size: u64, uncompressed_size: u64) {
        // Record the entry for this block
        self.entries.push(GziEntry {
            compressed_offset: self.current_compressed_offset,
            uncompressed_offset: self.current_uncompressed_offset,
        });

        // Update positions for next block
        self.current_compressed_offset += compressed_size;
        self.current_uncompressed_offset += uncompressed_size;
    }

    /// Get the current compressed offset (for tracking).
    pub fn compressed_offset(&self) -> u64 {
        self.current_compressed_offset
    }

    /// Get the current uncompressed offset (for tracking).
    pub fn uncompressed_offset(&self) -> u64 {
        self.current_uncompressed_offset
    }

    /// Get the number of entries in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries.
    pub fn entries(&self) -> &[GziEntry] {
        &self.entries
    }

    /// Write the GZI index to a writer.
    ///
    /// Format: number of entries (u64 LE), then pairs of (compressed, uncompressed) offsets.
    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        // Write number of entries
        writer.write_all(&(self.entries.len() as u64).to_le_bytes())?;

        // Write each entry
        for entry in &self.entries {
            writer.write_all(&entry.compressed_offset.to_le_bytes())?;
            writer.write_all(&entry.uncompressed_offset.to_le_bytes())?;
        }

        Ok(())
    }

    /// Reset the builder for reuse.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.current_compressed_offset = 0;
        self.current_uncompressed_offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gzi_builder_basic() {
        let mut builder = GziIndexBuilder::new();

        // Add some blocks
        builder.add_block(100, 1000); // Block 0: compressed 100 bytes, uncompressed 1000
        builder.add_block(150, 2000); // Block 1: compressed 150 bytes, uncompressed 2000
        builder.add_block(120, 1500); // Block 2: compressed 120 bytes, uncompressed 1500

        assert_eq!(builder.len(), 3);
        assert_eq!(builder.compressed_offset(), 370);
        assert_eq!(builder.uncompressed_offset(), 4500);

        let entries = builder.entries();
        assert_eq!(entries[0].compressed_offset, 0);
        assert_eq!(entries[0].uncompressed_offset, 0);
        assert_eq!(entries[1].compressed_offset, 100);
        assert_eq!(entries[1].uncompressed_offset, 1000);
        assert_eq!(entries[2].compressed_offset, 250);
        assert_eq!(entries[2].uncompressed_offset, 3000);
    }

    #[test]
    fn test_gzi_write() {
        let mut builder = GziIndexBuilder::new();
        builder.add_block(100, 1000);
        builder.add_block(200, 2000);

        let mut output = Vec::new();
        builder.write(&mut output).unwrap();

        // Should be: 8 bytes (count) + 2 * 16 bytes (entries) = 40 bytes
        assert_eq!(output.len(), 40);

        // Check count
        let count = u64::from_le_bytes(output[0..8].try_into().unwrap());
        assert_eq!(count, 2);

        // Check first entry
        let c0 = u64::from_le_bytes(output[8..16].try_into().unwrap());
        let u0 = u64::from_le_bytes(output[16..24].try_into().unwrap());
        assert_eq!(c0, 0);
        assert_eq!(u0, 0);

        // Check second entry
        let c1 = u64::from_le_bytes(output[24..32].try_into().unwrap());
        let u1 = u64::from_le_bytes(output[32..40].try_into().unwrap());
        assert_eq!(c1, 100);
        assert_eq!(u1, 1000);
    }
}
