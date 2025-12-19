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

/// Configuration for transcoding
#[derive(Clone, Debug)]
pub struct TranscodeConfig {
    /// Target uncompressed block size (default: 65280, max for BGZF)
    pub block_size: usize,
    /// Use fixed Huffman tables (faster) vs dynamic (better compression)
    pub use_fixed_huffman: bool,
    /// Number of threads for parallel encoding (0 = auto, 1 = single-threaded)
    pub num_threads: usize,
    /// Buffer size for I/O operations
    pub buffer_size: usize,
    /// Use thorough BGZF validation (validates all blocks vs just first)
    pub strict_bgzf_check: bool,
    /// Skip BGZF detection entirely (always transcode)
    pub force_transcode: bool,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            block_size: 65280,
            use_fixed_huffman: true,
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
