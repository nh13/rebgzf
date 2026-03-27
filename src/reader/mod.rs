//! Parallel gzip reader using speculative decode with marker-based window resolution.
//!
//! This module implements rapidgzip-style parallel decompression:
//! 1. Speculatively decode DEFLATE chunks without knowing the previous 32KB window
//! 2. Encode unknown back-references as 16-bit markers
//! 3. Replace markers in parallel once the window becomes available

mod chunk;
mod fetcher;
mod marker;
mod replacer;
mod speculative;
mod window_map;

pub use chunk::ChunkData;
pub use fetcher::{ChunkFetcher, FetcherConfig};
pub use marker::{apply_window, contains_markers, MarkerValue};
pub use replacer::replace_markers;
pub use speculative::{decode_with_libdeflate, speculative_decode};
pub use window_map::WindowMap;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// Parallel gzip reader that transparently decompresses using multiple threads.
///
/// For regular files, the input is memory-mapped and split into chunks that are
/// decoded in parallel via speculative DEFLATE decompression. For non-seekable
/// inputs (pipes, sockets), falls back to single-threaded streaming via flate2.
///
/// Multi-member gzip files are decompressed in batches to cap peak memory usage.
/// Only `batch_size` members are decompressed at a time; completed batches are
/// freed as they are consumed by the caller.
pub struct ParallelGzipReader {
    inner: ReaderInner,
    buffer: Vec<u8>,
    buffer_pos: usize,
}

enum ReaderInner {
    /// Multi-member gzip: decompress batches of members on demand.
    MultiMember {
        /// Memory-mapped file data (kept alive for the reader's lifetime).
        mmap: memmap2::Mmap,
        /// Byte-offset boundaries for each member: `(start, end)`.
        members: Vec<(usize, usize)>,
        /// Index of the next member to start decoding in the next batch.
        next_batch_start: usize,
        /// Number of members to decode per batch.
        batch_size: usize,
        /// Number of worker threads.
        num_threads: usize,
        /// Whether to verify CRC32 after decompression.
        verify_crc: bool,
        /// Decoded chunks waiting to be consumed (FIFO).
        pending_chunks: VecDeque<ChunkData>,
    },
    /// Single-member gzip: all chunks pre-decoded via speculative parallel decode.
    SingleMember {
        /// Memory map kept alive for safety.
        _mmap: memmap2::Mmap,
        /// Pre-decoded chunk fetcher.
        fetcher: fetcher::ChunkFetcher,
    },
    /// Streaming fallback (pipes, sockets).
    Streaming(Box<dyn Read + Send>),
}

impl ParallelGzipReader {
    /// Create from a file path.
    ///
    /// Memory-maps the file if it is a regular file with nonzero size, otherwise
    /// falls back to streaming decompression. Multi-member gzip files (e.g. from
    /// `pigz`) are detected automatically and each member is decompressed lazily
    /// in parallel batches to cap peak memory usage.
    ///
    /// # Arguments
    /// * `path` - path to the gzip file
    /// * `threads` - number of worker threads (0 = auto-detect)
    pub fn from_file<P: AsRef<Path>>(path: P, threads: usize) -> io::Result<Self> {
        let file = File::open(path.as_ref())?;
        let metadata = file.metadata()?;

        if metadata.is_file() && metadata.len() > 0 {
            // Safety: the file is a regular file that we keep open for the lifetime
            // of the mmap. We do not write to the mmap.
            let mmap = unsafe { memmap2::Mmap::map(&file)? };

            let threads = if threads == 0 { num_cpus::get() } else { threads };
            let config = FetcherConfig { threads, chunk_size: 4 * 1024 * 1024, verify_crc: true };

            let members = fetcher::scan_gzip_members(mmap.as_ref());

            if members.len() > 1 {
                // Multi-member: set up lazy batch decoding.
                let batch_size = threads * 64;
                Ok(Self {
                    inner: ReaderInner::MultiMember {
                        mmap,
                        members,
                        next_batch_start: 0,
                        batch_size,
                        num_threads: threads,
                        verify_crc: config.verify_crc,
                        pending_chunks: VecDeque::new(),
                    },
                    buffer: Vec::new(),
                    buffer_pos: 0,
                })
            } else {
                // Single member: pre-decode everything via speculative decode.
                let fetcher = fetcher::ChunkFetcher::from_data(mmap.as_ref(), config)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

                Ok(Self {
                    inner: ReaderInner::SingleMember { _mmap: mmap, fetcher },
                    buffer: Vec::new(),
                    buffer_pos: 0,
                })
            }
        } else {
            Self::from_reader(file, threads)
        }
    }

    /// Create from any `Read` source (streaming fallback with flate2).
    ///
    /// The `_threads` parameter is accepted for API symmetry but ignored;
    /// streaming decompression is single-threaded.
    pub fn from_reader<R: Read + Send + 'static>(reader: R, _threads: usize) -> io::Result<Self> {
        let decoder = flate2::read::MultiGzDecoder::new(BufReader::new(reader));
        Ok(Self {
            inner: ReaderInner::Streaming(Box::new(decoder)),
            buffer: Vec::new(),
            buffer_pos: 0,
        })
    }
}

impl Read for ParallelGzipReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Drain current buffer first.
        if self.buffer_pos < self.buffer.len() {
            let available = self.buffer.len() - self.buffer_pos;
            let to_copy = available.min(buf.len());
            buf[..to_copy]
                .copy_from_slice(&self.buffer[self.buffer_pos..self.buffer_pos + to_copy]);
            self.buffer_pos += to_copy;
            return Ok(to_copy);
        }

        match &mut self.inner {
            ReaderInner::MultiMember {
                mmap,
                members,
                next_batch_start,
                batch_size,
                num_threads,
                verify_crc,
                pending_chunks,
            } => {
                // Try to find the next non-empty chunk, fetching new batches as needed.
                loop {
                    // Drain pending chunks, skipping empty ones.
                    while let Some(mut chunk) = pending_chunks.pop_front() {
                        self.buffer.clear();
                        for segment in chunk.take_output() {
                            self.buffer.extend_from_slice(&segment);
                        }
                        if !self.buffer.is_empty() {
                            self.buffer_pos = 0;
                            let to_copy = self.buffer.len().min(buf.len());
                            buf[..to_copy].copy_from_slice(&self.buffer[..to_copy]);
                            self.buffer_pos = to_copy;
                            return Ok(to_copy);
                        }
                    }

                    // No pending chunks — decode the next batch.
                    if *next_batch_start >= members.len() {
                        return Ok(0); // EOF
                    }

                    let batch_end = (*next_batch_start + *batch_size).min(members.len());
                    let batch_members = &members[*next_batch_start..batch_end];

                    let new_chunks = fetcher::decode_member_batch(
                        mmap.as_ref(),
                        batch_members,
                        *num_threads,
                        *verify_crc,
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

                    *next_batch_start = batch_end;
                    pending_chunks.extend(new_chunks);
                }
            }
            ReaderInner::SingleMember { fetcher, .. } => {
                loop {
                    match fetcher.next_chunk() {
                        Some(mut chunk) => {
                            // Flatten chunk output into buffer.
                            self.buffer.clear();
                            for segment in chunk.take_output() {
                                self.buffer.extend_from_slice(&segment);
                            }
                            if !self.buffer.is_empty() {
                                self.buffer_pos = 0;
                                let to_copy = self.buffer.len().min(buf.len());
                                buf[..to_copy].copy_from_slice(&self.buffer[..to_copy]);
                                self.buffer_pos = to_copy;
                                return Ok(to_copy);
                            }
                            // Empty chunk — continue to next
                        }
                        None => return Ok(0),
                    }
                }
            }
            ReaderInner::Streaming(reader) => reader.read(buf),
        }
    }
}
