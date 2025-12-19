//! Parallel transcoder implementation using a producer-consumer pipeline.
//!
//! Architecture:
//! - Main thread: Parse DEFLATE, accumulate tokens, resolve boundaries, send jobs
//! - Worker pool: Encode tokens to BGZF blocks in parallel
//! - Main thread: Receive encoded blocks in order, write to output

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};

use crossbeam::channel::{bounded, Receiver, Sender};

use super::boundary::{tokens_to_bytes, BoundaryResolver};
use crate::bgzf::BGZF_EOF;
use crate::deflate::{DeflateParser, LZ77Token};
use crate::error::{Error, Result};
use crate::gzip::GzipHeader;
use crate::huffman::HuffmanEncoder;
use crate::{TranscodeConfig, TranscodeStats, Transcoder};

/// A job for encoding a single BGZF block
struct EncodingJob {
    /// Sequence number for ordering output
    block_id: u64,
    /// Resolved LZ77 tokens for this block
    tokens: Vec<LZ77Token>,
    /// Uncompressed size (pre-computed during boundary resolution)
    uncompressed_size: u32,
}

/// Result of encoding a single BGZF block
struct EncodedBlock {
    /// Sequence number for ordering output
    block_id: u64,
    /// Raw BGZF block data (header + deflate + footer)
    data: Vec<u8>,
}

/// Parallel transcoder implementation
pub struct ParallelTranscoder {
    config: TranscodeConfig,
}

impl ParallelTranscoder {
    pub fn new(config: TranscodeConfig) -> Self {
        Self { config }
    }

    fn effective_threads(&self) -> usize {
        match self.config.num_threads {
            0 => num_cpus::get().clamp(1, 32),
            n => n.clamp(1, 32),
        }
    }
}

impl Transcoder for ParallelTranscoder {
    fn transcode<R: Read, W: Write>(&mut self, input: R, output: W) -> Result<TranscodeStats> {
        let num_threads = self.effective_threads();

        // For single thread, delegate to single-threaded implementation for efficiency
        if num_threads == 1 {
            let mut single = super::single::SingleThreadedTranscoder::new(self.config.clone());
            return single.transcode(input, output);
        }

        self.transcode_parallel(input, output, num_threads)
    }
}

impl ParallelTranscoder {
    fn transcode_parallel<R: Read, W: Write>(
        &mut self,
        input: R,
        mut output: W,
        num_threads: usize,
    ) -> Result<TranscodeStats> {
        // Channel capacity - enough to keep workers busy without excessive memory
        let channel_capacity = num_threads * 4;

        // Channels for job distribution
        let (job_tx, job_rx): (Sender<EncodingJob>, Receiver<EncodingJob>) =
            bounded(channel_capacity);
        let (result_tx, result_rx): (Sender<Result<EncodedBlock>>, Receiver<Result<EncodedBlock>>) =
            bounded(channel_capacity);

        // Shared config for workers
        let use_fixed_huffman = self.config.use_fixed_huffman();

        // Use crossbeam's scoped threads to avoid 'static lifetime requirements
        let result = crossbeam::scope(|scope| {
            // Spawn worker threads
            for _ in 0..num_threads {
                let job_rx = job_rx.clone();
                let result_tx = result_tx.clone();

                scope.spawn(move |_| {
                    worker_thread(job_rx, result_tx, use_fixed_huffman);
                });
            }

            // Drop our copies of the channels that workers use
            drop(job_rx);
            drop(result_tx);

            // Parse and send jobs on main thread, interleaved with receiving results
            self.parse_dispatch_and_write(input, &mut output, job_tx, result_rx)
        });

        // Unwrap scope result
        result.map_err(|_| Error::Internal("Thread panicked".to_string()))?
    }

    fn parse_dispatch_and_write<R: Read, W: Write>(
        &self,
        input: R,
        output: &mut W,
        job_tx: Sender<EncodingJob>,
        result_rx: Receiver<Result<EncodedBlock>>,
    ) -> Result<TranscodeStats> {
        let mut reader = BufReader::with_capacity(self.config.buffer_size, input);
        let mut writer = BufWriter::with_capacity(self.config.buffer_size, output);

        // Parse gzip header
        let _gzip_header = GzipHeader::parse(&mut reader)?;

        // Initialize components
        let mut parser = DeflateParser::new(&mut reader);
        let mut resolver = BoundaryResolver::new();

        // Accumulator for current BGZF block
        let mut pending_tokens: Vec<LZ77Token> = Vec::with_capacity(8192);
        let mut pending_uncompressed_size: usize = 0;
        let mut block_start_position: u64 = 0;
        let mut next_block_id: u64 = 0;

        // Stats
        let mut blocks_written: u64 = 0;
        let mut output_bytes: u64 = 0;

        // Buffer for out-of-order blocks
        let mut pending_blocks: BTreeMap<u64, EncodedBlock> = BTreeMap::new();
        let mut next_write_id: u64 = 0;

        // Main parsing loop - handles multiple gzip members
        loop {
            // Process all DEFLATE blocks in current gzip member
            while let Some(deflate_block) = parser.parse_block()? {
                // Take ownership of tokens to avoid cloning
                for token in deflate_block.tokens {
                    if matches!(token, LZ77Token::EndOfBlock) {
                        continue;
                    }

                    let token_size = token.uncompressed_size();

                    // Check if adding this token would exceed BGZF block size
                    if pending_uncompressed_size + token_size > self.config.block_size
                        && !pending_tokens.is_empty()
                    {
                        // Resolve boundaries only - workers will compute CRC in parallel
                        let (resolved, uncompressed_size) = resolver
                            .resolve_block_for_parallel(block_start_position, &pending_tokens);

                        let job = EncodingJob {
                            block_id: next_block_id,
                            tokens: resolved,
                            uncompressed_size,
                        };
                        next_block_id += 1;

                        // Send job, draining results as needed to prevent deadlock
                        let mut job_to_send = Some(job);
                        while job_to_send.is_some() {
                            crossbeam::channel::select! {
                                send(job_tx, job_to_send.clone().unwrap()) -> res => {
                                    match res {
                                        Ok(()) => { job_to_send = None; }
                                        Err(_) => {
                                            return Err(Error::Internal("Workers disconnected".to_string()));
                                        }
                                    }
                                }
                                recv(result_rx) -> res => {
                                    match res {
                                        Ok(result) => {
                                            let block = result?;
                                            Self::buffer_and_write_block(
                                                &mut writer,
                                                block,
                                                &mut pending_blocks,
                                                &mut next_write_id,
                                                &mut blocks_written,
                                                &mut output_bytes,
                                            )?;
                                        }
                                        Err(_) => {
                                            return Err(Error::Internal(
                                                "Result channel disconnected".to_string(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }

                        block_start_position = resolver.position();
                        pending_tokens.clear();
                        pending_uncompressed_size = 0;
                    }

                    // No clone needed - we own the token
                    pending_tokens.push(token);
                    pending_uncompressed_size += token_size;
                }
            }

            // Check for another gzip member
            if !parser.read_trailer_and_check_next()? {
                break; // No more members, we're done
            }
            // Continue with next member - parser state has been reset
        }

        // Flush remaining tokens
        if !pending_tokens.is_empty() {
            let (resolved, uncompressed_size) =
                resolver.resolve_block_for_parallel(block_start_position, &pending_tokens);

            let job = EncodingJob { block_id: next_block_id, tokens: resolved, uncompressed_size };
            next_block_id += 1;

            let _ = job_tx.send(job);
        }

        // Drop job_tx to signal workers we're done
        drop(job_tx);

        // Drain remaining results
        while blocks_written + (pending_blocks.len() as u64) < next_block_id {
            match result_rx.recv() {
                Ok(result) => {
                    let block = result?;
                    Self::buffer_and_write_block(
                        &mut writer,
                        block,
                        &mut pending_blocks,
                        &mut next_write_id,
                        &mut blocks_written,
                        &mut output_bytes,
                    )?;
                }
                Err(_) => break,
            }
        }

        // Write any remaining buffered blocks
        while let Some(block) = pending_blocks.remove(&next_write_id) {
            output_bytes += block.data.len() as u64;
            writer.write_all(&block.data)?;
            blocks_written += 1;
            next_write_id += 1;
        }

        // Write EOF marker
        writer.write_all(&BGZF_EOF)?;
        output_bytes += 28;

        writer.flush()?;

        let (refs_resolved, _refs_preserved) = resolver.stats();

        Ok(TranscodeStats {
            input_bytes: parser.bytes_read(),
            output_bytes,
            blocks_written,
            boundary_refs_resolved: refs_resolved,
            copied_directly: false,
        })
    }

    fn buffer_and_write_block<W: Write>(
        writer: &mut W,
        block: EncodedBlock,
        pending: &mut BTreeMap<u64, EncodedBlock>,
        next_write_id: &mut u64,
        blocks_written: &mut u64,
        output_bytes: &mut u64,
    ) -> Result<()> {
        if block.block_id == *next_write_id {
            // Write this block
            *output_bytes += block.data.len() as u64;
            writer.write_all(&block.data)?;
            *blocks_written += 1;
            *next_write_id += 1;

            // Write any consecutive buffered blocks
            while let Some(buffered) = pending.remove(next_write_id) {
                *output_bytes += buffered.data.len() as u64;
                writer.write_all(&buffered.data)?;
                *blocks_written += 1;
                *next_write_id += 1;
            }
        } else {
            // Buffer out-of-order block
            pending.insert(block.block_id, block);
        }
        Ok(())
    }
}

// Need Clone for EncodingJob to handle retry in try_send
impl Clone for EncodingJob {
    fn clone(&self) -> Self {
        Self {
            block_id: self.block_id,
            tokens: self.tokens.clone(),
            uncompressed_size: self.uncompressed_size,
        }
    }
}

/// Worker thread function: encodes tokens to BGZF blocks
fn worker_thread(
    job_rx: Receiver<EncodingJob>,
    result_tx: Sender<Result<EncodedBlock>>,
    use_fixed_huffman: bool,
) {
    let mut encoder = HuffmanEncoder::new(use_fixed_huffman);

    while let Ok(job) = job_rx.recv() {
        let result = encode_block(&mut encoder, job);

        if result_tx.send(result).is_err() {
            // Main thread has stopped, exit
            break;
        }
    }
}

/// Encode a single BGZF block
fn encode_block(encoder: &mut HuffmanEncoder, job: EncodingJob) -> Result<EncodedBlock> {
    // Compute CRC from tokens (parallelized - each worker does this)
    let uncompressed_bytes = tokens_to_bytes(&job.tokens);
    let crc = crc32fast::hash(&uncompressed_bytes);
    let isize = job.uncompressed_size;

    // Encode to DEFLATE
    let deflate_data = encoder.encode(&job.tokens, true)?;

    // Build complete BGZF block
    let block_size = 18 + deflate_data.len() + 8; // header + deflate + footer
    let bsize = block_size - 1;

    let mut data = Vec::with_capacity(block_size);

    // Header
    data.extend_from_slice(&[
        0x1f,
        0x8b, // gzip magic
        0x08, // compression method (DEFLATE)
        0x04, // flags (FEXTRA)
        0x00,
        0x00,
        0x00,
        0x00, // mtime
        0x00, // extra flags
        0xff, // OS (unknown)
        0x06,
        0x00, // xlen = 6
        0x42,
        0x43, // subfield ID "BC"
        0x02,
        0x00, // subfield length = 2
        (bsize & 0xFF) as u8,
        ((bsize >> 8) & 0xFF) as u8,
    ]);

    // Deflate data
    data.extend_from_slice(&deflate_data);

    // Footer: CRC32 + ISIZE
    data.extend_from_slice(&crc.to_le_bytes());
    data.extend_from_slice(&isize.to_le_bytes());

    Ok(EncodedBlock { block_id: job.block_id, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parallel_transcode() {
        use std::io::Write as IoWrite;

        // Create a gzip file with some data
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(b"Hello, World! This is some test data for parallel transcoding.")
            .unwrap();
        let gzip_data = encoder.finish().unwrap();

        // Transcode with 2 threads
        let config = TranscodeConfig { num_threads: 2, ..Default::default() };
        let mut transcoder = ParallelTranscoder::new(config);

        let mut output = Vec::new();
        let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

        assert!(stats.blocks_written >= 1);
        assert!(!output.is_empty());

        // Verify BGZF header
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[3] & 0x04, 0x04);
        assert_eq!(output[12], b'B');
        assert_eq!(output[13], b'C');
    }

    #[test]
    fn test_effective_threads() {
        let config = TranscodeConfig { num_threads: 0, ..Default::default() };
        let transcoder = ParallelTranscoder::new(config);
        let threads = transcoder.effective_threads();
        assert!(threads >= 1);
        assert!(threads <= 32);

        let config2 = TranscodeConfig { num_threads: 100, ..Default::default() };
        let transcoder2 = ParallelTranscoder::new(config2);
        assert_eq!(transcoder2.effective_threads(), 32); // Capped at 32
    }
}
