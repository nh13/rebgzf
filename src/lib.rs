pub mod bgzf;
pub mod bits;
pub mod deflate;
pub mod error;
pub mod gzip;
pub mod huffman;
pub mod transcoder;

pub use bgzf::{is_bgzf, validate_bgzf_strict, BgzfValidation};
pub use deflate::tokens::LZ77Token;
pub use error::{Error, Result};
pub use transcoder::{parallel::ParallelTranscoder, single::SingleThreadedTranscoder};

use std::io::{Read, Write};
use std::path::Path;

/// Compression level for encoding (1-9)
///
/// - Levels 1-3: Fixed Huffman tables (fastest, larger output)
/// - Levels 4-6: Dynamic Huffman per-block (balanced)
/// - Levels 7-9: Dynamic Huffman with smart boundary splitting (best compression)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CompressionLevel {
    #[default]
    Level1 = 1,
    Level2 = 2,
    Level3 = 3,
    Level4 = 4,
    Level5 = 5,
    Level6 = 6,
    Level7 = 7,
    Level8 = 8,
    Level9 = 9,
}

impl CompressionLevel {
    /// Create from numeric level (1-9), clamped to valid range
    pub fn from_level(level: u8) -> Self {
        match level {
            0 | 1 => Self::Level1,
            2 => Self::Level2,
            3 => Self::Level3,
            4 => Self::Level4,
            5 => Self::Level5,
            6 => Self::Level6,
            7 => Self::Level7,
            8 => Self::Level8,
            _ => Self::Level9,
        }
    }

    /// Get numeric level (1-9)
    pub fn level(&self) -> u8 {
        *self as u8
    }

    /// Whether this level uses fixed Huffman tables (levels 1-3)
    pub fn use_fixed_huffman(&self) -> bool {
        matches!(self, Self::Level1 | Self::Level2 | Self::Level3)
    }

    /// Whether this level uses smart boundary splitting (levels 7-9)
    pub fn use_smart_boundaries(&self) -> bool {
        matches!(self, Self::Level7 | Self::Level8 | Self::Level9)
    }
}

/// Format profile for input-aware optimization
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FormatProfile {
    /// Default encoding (fixed Huffman, simple boundaries)
    #[default]
    Default,
    /// FASTQ-optimized (dynamic Huffman, record-aligned boundaries)
    Fastq,
    /// Auto-detect from file extension
    Auto,
}

impl FormatProfile {
    /// Detect format from file path extension
    pub fn detect_from_path(path: &Path) -> Self {
        let name =
            path.file_name().and_then(|s| s.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();

        if name.ends_with(".fastq.gz") || name.ends_with(".fq.gz") {
            Self::Fastq
        } else {
            Self::Default
        }
    }

    /// Resolve Auto to a concrete profile based on path
    pub fn resolve(self, path: Option<&Path>) -> Self {
        match self {
            Self::Auto => path.map(Self::detect_from_path).unwrap_or(Self::Default),
            other => other,
        }
    }
}

/// Configuration for transcoding
#[derive(Clone, Debug)]
pub struct TranscodeConfig {
    /// Target uncompressed block size (default: 65280, max for BGZF)
    pub block_size: usize,
    /// Compression level (1-9)
    pub compression_level: CompressionLevel,
    /// Format profile for input-aware optimization
    pub format: FormatProfile,
    /// Number of threads for parallel encoding (0 = auto, 1 = single-threaded)
    pub num_threads: usize,
    /// Buffer size for I/O operations
    pub buffer_size: usize,
    /// Use thorough BGZF validation (validates all blocks vs just first)
    pub strict_bgzf_check: bool,
    /// Skip BGZF detection entirely (always transcode)
    pub force_transcode: bool,
}

impl TranscodeConfig {
    /// Whether to use fixed Huffman tables based on compression level
    pub fn use_fixed_huffman(&self) -> bool {
        self.compression_level.use_fixed_huffman()
    }

    /// Whether to use smart boundary splitting based on level and format
    pub fn use_smart_boundaries(&self) -> bool {
        self.compression_level.use_smart_boundaries() || self.format == FormatProfile::Fastq
    }
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            block_size: 65280,
            compression_level: CompressionLevel::Level1,
            format: FormatProfile::Default,
            num_threads: 0,
            buffer_size: 128 * 1024,
            strict_bgzf_check: false,
            force_transcode: false,
        }
    }
}

/// Statistics from a transcoding operation
#[derive(Clone, Debug, Default)]
pub struct TranscodeStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub blocks_written: u64,
    pub boundary_refs_resolved: u64,
    /// Input was already valid BGZF and was copied directly
    pub copied_directly: bool,
}

/// Trait for the complete transcoding operation
pub trait Transcoder {
    /// Transcode from gzip input to BGZF output
    fn transcode<R: Read, W: Write>(&mut self, input: R, output: W) -> Result<TranscodeStats>;
}
