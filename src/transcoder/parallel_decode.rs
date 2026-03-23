//! Parallel DEFLATE decode transcoder using pugz-style block scanning.
//!
//! Pipeline:
//! 1. Parse gzip header, compute DEFLATE region
//! 2. Divide region into N byte-offset chunks
//! 3. Phase 1 (parallel): Scan for DEFLATE block boundaries at chunk starts
//! 4. Phase 2 (parallel): Each thread decodes Huffman symbols from its region,
//!    emitting raw LZ77 tokens (Literal, Copy, EndOfBlock) without a sliding window
//! 5. Phase 3 (sequential): Feed all tokens through BoundaryResolver to resolve
//!    cross-boundary references, then encode and emit BGZF blocks

use std::collections::BTreeMap;
use std::io::{BufWriter, Write};

use crossbeam::channel::{bounded, Receiver, Sender};

use super::block_scanner::scan_for_block;
use super::boundary::{tokens_to_bytes, BoundaryResolver};
use super::single::{parse_gzip_header_size, SingleThreadedTranscoder};
use crate::bgzf::{GziEntry, BGZF_EOF};
use crate::bits::{BitRead, SliceBitReader};
use crate::deflate::parser::parse_dynamic_huffman_tables;
use crate::deflate::tables::{DISTANCE_TABLE, LENGTH_TABLE};
use crate::deflate::LZ77Token;
use crate::error::{Error, Result};
use crate::huffman::{HuffmanDecoder, HuffmanEncoder};
use crate::{TranscodeConfig, TranscodeStats};

/// Minimum DEFLATE region size (in bytes) to justify parallelism.
const MIN_REGION_BYTES: usize = 512 * 1024;

/// How far (in bytes) each thread scans looking for a valid DEFLATE block boundary.
const SCAN_WINDOW_BYTES: usize = 1024 * 1024;

/// Parallel DEFLATE decode transcoder.
pub struct ParallelDecodeTranscoder {
    config: TranscodeConfig,
    min_region_bytes: usize,
}

impl ParallelDecodeTranscoder {
    pub fn new(config: TranscodeConfig) -> Self {
        Self { config, min_region_bytes: MIN_REGION_BYTES }
    }

    /// Override the minimum DEFLATE region size required for parallelism (for testing).
    #[cfg(test)]
    fn with_min_region_bytes(mut self, min_region_bytes: usize) -> Self {
        self.min_region_bytes = min_region_bytes;
        self
    }

    /// Transcode from a memory-mapped gzip byte slice to a writer.
    ///
    /// Combined scan+decode approach (rapidgzip-style):
    /// Each thread scans for candidate block boundaries AND decodes from them.
    /// If decoding from a candidate fails or produces too few tokens, the thread
    /// tries the next candidate. This eliminates false positive boundaries.
    ///
    /// **Limitation**: assumes the input is a single-member gzip file. For multi-member
    /// gzip (e.g. concatenated `.gz` files), `deflate_end = data.len() - 8` only locates
    /// the trailer of the *last* member, causing the parallel decoder to treat intermediate
    /// member boundaries as part of the DEFLATE stream. Multi-member files fall back to the
    /// single-threaded transcoder (via `region < MIN_REGION_BYTES` or thread count 1),
    /// which correctly handles multi-member via `read_trailer_and_check_next`.
    pub fn transcode_mmap<W: Write>(&mut self, data: &[u8], output: W) -> Result<TranscodeStats> {
        let header_size = parse_gzip_header_size(data)?;
        let deflate_end = data.len().saturating_sub(8);
        let num_threads = self.effective_threads();

        let region = deflate_end.saturating_sub(header_size);
        if region < self.min_region_bytes || num_threads <= 1 {
            return self.fallback(data, output);
        }

        let chunk_size = region / num_threads;

        // Combined Phase 1+2: Each thread scans for a valid boundary and decodes.
        // Thread 0 starts at the known DEFLATE start (no scanning needed).
        // Threads 1..N scan their chunk start region for candidates, try decoding
        // from each candidate, and keep the first one that produces enough tokens.
        let chunk_tokens =
            self.scan_and_decode_all(data, header_size, chunk_size, num_threads, deflate_end)?;

        if chunk_tokens.is_empty() {
            return self.fallback(data, output);
        }

        // Phase 3: Resolve boundaries sequentially, encode BGZF in parallel
        self.emit_bgzf(data, chunk_tokens, output)
    }

    /// Phase 3: Resolve boundaries sequentially, encode BGZF blocks in parallel.
    ///
    /// Main thread: iterate tokens → BoundaryResolver → send resolved blocks to workers
    /// Workers: tokens_to_bytes → CRC → HuffmanEncoder → build BGZF block
    /// Main thread: receive encoded blocks in order → write to output
    fn emit_bgzf<W: Write>(
        &self,
        data: &[u8],
        chunk_tokens: Vec<Vec<LZ77Token>>,
        output: W,
    ) -> Result<TranscodeStats> {
        let num_threads = self.effective_threads();
        let channel_capacity = num_threads * 4;
        let use_fixed_huffman = self.config.use_fixed_huffman();

        let (job_tx, job_rx): (Sender<EncodingJob>, Receiver<EncodingJob>) =
            bounded(channel_capacity);
        let (result_tx, result_rx): (Sender<Result<EncodedBlock>>, Receiver<Result<EncodedBlock>>) =
            bounded(channel_capacity);

        let result = crossbeam::scope(|scope| {
            // Spawn encoding worker threads
            for _ in 0..num_threads {
                let rx = job_rx.clone();
                let tx = result_tx.clone();
                scope.spawn(move |_| {
                    encoding_worker(rx, tx, use_fixed_huffman);
                });
            }
            drop(job_rx);
            drop(result_tx);

            // Main thread: resolve + dispatch + collect + write
            self.resolve_dispatch_write(data, chunk_tokens, job_tx, result_rx, output)
        });

        result.map_err(|_| Error::Internal("Phase 3 thread panicked".into()))?
    }

    /// Main thread work for Phase 3: resolve boundaries, dispatch to workers, write output.
    fn resolve_dispatch_write<W: Write>(
        &self,
        data: &[u8],
        chunk_tokens: Vec<Vec<LZ77Token>>,
        job_tx: Sender<EncodingJob>,
        result_rx: Receiver<Result<EncodedBlock>>,
        output: W,
    ) -> Result<TranscodeStats> {
        let mut writer = BufWriter::with_capacity(self.config.buffer_size, output);
        let mut resolver = BoundaryResolver::new();

        let mut pending_tokens: Vec<LZ77Token> = Vec::with_capacity(32768);
        let mut pending_uncompressed_size: usize = 0;
        let mut block_start_position: u64 = 0;
        let mut next_block_id: u64 = 0;

        // Output ordering state
        let build_index = self.config.build_index;
        let mut blocks_written: u64 = 0;
        let mut output_bytes: u64 = 0;
        let mut index_entries: Vec<GziEntry> = Vec::new();
        let mut current_compressed_offset: u64 = 0;
        let mut current_uncompressed_offset: u64 = 0;
        let mut pending_blocks: BTreeMap<u64, EncodedBlock> = BTreeMap::new();
        let mut next_write_id: u64 = 0;

        // Iterate all tokens from all chunks, accumulating into pending_tokens.
        // Use into_iter to take ownership (avoids clone).
        for chunk in chunk_tokens {
            for token in chunk {
                if matches!(token, LZ77Token::EndOfBlock) {
                    continue;
                }

                let token_size = token.uncompressed_size();

                let should_emit = pending_uncompressed_size + token_size > self.config.block_size
                    && !pending_tokens.is_empty();

                if should_emit {
                    let (resolved, uncompressed_size) =
                        resolver.resolve_block_for_parallel(block_start_position, &pending_tokens);

                    let job = EncodingJob {
                        block_id: next_block_id,
                        tokens: resolved,
                        uncompressed_size,
                    };
                    next_block_id += 1;

                    send_job_and_drain(
                        &job_tx,
                        &result_rx,
                        job,
                        &mut writer,
                        &mut pending_blocks,
                        &mut next_write_id,
                        &mut blocks_written,
                        &mut output_bytes,
                        build_index,
                        &mut index_entries,
                        &mut current_compressed_offset,
                        &mut current_uncompressed_offset,
                    )?;

                    block_start_position = resolver.position();
                    pending_tokens.clear();
                    pending_uncompressed_size = 0;
                }

                pending_uncompressed_size += token_size;
                pending_tokens.push(token); // moved, not cloned
            }
        }

        // Flush remaining tokens (must use send_job_and_drain to avoid deadlock —
        // a blocking send here can deadlock if both channels are full and workers
        // are blocked on result_tx.send while the main thread blocks on job_tx.send)
        if !pending_tokens.is_empty() {
            let (resolved, uncompressed_size) =
                resolver.resolve_block_for_parallel(block_start_position, &pending_tokens);
            let job = EncodingJob { block_id: next_block_id, tokens: resolved, uncompressed_size };
            next_block_id += 1;
            send_job_and_drain(
                &job_tx,
                &result_rx,
                job,
                &mut writer,
                &mut pending_blocks,
                &mut next_write_id,
                &mut blocks_written,
                &mut output_bytes,
                build_index,
                &mut index_entries,
                &mut current_compressed_offset,
                &mut current_uncompressed_offset,
            )?;
        }

        // Signal workers to stop
        drop(job_tx);

        // Drain remaining results
        while blocks_written + (pending_blocks.len() as u64) < next_block_id {
            match result_rx.recv() {
                Ok(result) => {
                    let block = result?;
                    buffer_and_write_block(
                        &mut writer,
                        block,
                        &mut pending_blocks,
                        &mut next_write_id,
                        &mut blocks_written,
                        &mut output_bytes,
                        build_index,
                        &mut index_entries,
                        &mut current_compressed_offset,
                        &mut current_uncompressed_offset,
                    )?;
                }
                Err(_) => break,
            }
        }

        // Write any remaining buffered blocks
        while let Some(block) = pending_blocks.remove(&next_write_id) {
            write_single_block(
                &mut writer,
                &block.data,
                block.uncompressed_size,
                &mut output_bytes,
                build_index,
                &mut index_entries,
                &mut current_compressed_offset,
                &mut current_uncompressed_offset,
            )?;
            blocks_written += 1;
            next_write_id += 1;
        }

        // Write EOF
        writer.write_all(&BGZF_EOF).map_err(Error::Io)?;
        output_bytes += 28;
        writer.flush().map_err(Error::Io)?;

        let (resolved, _) = resolver.stats();

        Ok(TranscodeStats {
            input_bytes: data.len() as u64,
            output_bytes,
            blocks_written,
            boundary_refs_resolved: resolved,
            copied_directly: false,
            index_entries: if build_index { Some(index_entries) } else { None },
        })
    }

    /// Combined scan + decode for all chunks in parallel.
    ///
    /// Each thread (except thread 0) scans for candidate boundaries and tries
    /// decoding from each until one produces a substantial token count.
    /// Returns token vectors for each successful chunk, in order.
    ///
    /// **Note on chunk ownership**: Thread K's `stop_byte` is the nominal chunk boundary
    /// `header_size + (K+1) * chunk_size`, while thread K+1's start is its discovered
    /// DEFLATE block boundary (which may be slightly before or after that nominal position).
    /// In theory this can produce a small overlap or gap in decoded tokens. In practice,
    /// the sequential `BoundaryResolver` in Phase 3 processes all tokens in order and
    /// resolves cross-boundary LZ77 references correctly, so minor overlap is harmless.
    fn scan_and_decode_all(
        &self,
        data: &[u8],
        header_size: usize,
        chunk_size: usize,
        num_threads: usize,
        deflate_end: usize,
    ) -> Result<Vec<Vec<LZ77Token>>> {
        // Each thread returns (boundary_bit, tokens)
        type ChunkResult = (usize, Vec<LZ77Token>);
        let mut results: Vec<Option<ChunkResult>> = (0..num_threads).map(|_| None).collect();

        crossbeam::scope(|scope| {
            let mut handles = Vec::with_capacity(num_threads);

            for k in 0..num_threads {
                let stop_byte = if k + 1 < num_threads {
                    header_size + (k + 1) * chunk_size
                } else {
                    deflate_end
                };

                handles.push((
                    k,
                    scope.spawn(move |_| -> Option<ChunkResult> {
                        if k == 0 {
                            // Thread 0: known start, decode until chunk end
                            let start_bit = header_size * 8;
                            let stop_bit = stop_byte * 8;
                            match decode_chunk_tokens(data, start_bit, stop_bit) {
                                Ok(tokens) => Some((start_bit, tokens)),
                                Err(_) => None,
                            }
                        } else {
                            // Thread K>0: scan for candidates, try decode from each
                            scan_and_decode_chunk(
                                data,
                                header_size + k * chunk_size,
                                stop_byte,
                                deflate_end,
                            )
                        }
                    }),
                ));
            }

            for (k, handle) in handles {
                if let Ok(result) = handle.join() {
                    results[k] = result;
                }
            }
        })
        .map_err(|_| Error::Internal("Parallel scan+decode thread panicked".into()))?;

        // Collect successful chunks in order
        let mut chunk_tokens = Vec::new();
        for result in results.into_iter().flatten() {
            let (_, tokens) = result;
            if !tokens.is_empty() {
                chunk_tokens.push(tokens);
            }
        }

        Ok(chunk_tokens)
    }

    fn fallback<W: Write>(&self, data: &[u8], output: W) -> Result<TranscodeStats> {
        let mut single = SingleThreadedTranscoder::new(self.config.clone());
        single.transcode_slice(data, output)
    }

    fn effective_threads(&self) -> usize {
        match self.config.num_threads {
            0 => num_cpus::get().clamp(1, 32),
            n => n.clamp(1, 32),
        }
    }
}

/// Minimum tokens a probe decode must produce to accept a candidate boundary.
const MIN_PROBE_TOKENS: usize = 1000;

/// How far (in bytes) to probe-decode before deciding if a candidate is valid.
/// 64KB of compressed data should produce ~30-100K tokens if the boundary is real.
const PROBE_BYTES: usize = 64 * 1024;

/// Scan for candidate boundaries and try decoding from each (rapidgzip-style).
///
/// For each structural candidate:
/// 1. Probe-decode up to PROBE_BYTES of compressed data
/// 2. If probe produces >= MIN_PROBE_TOKENS → candidate is real, do full decode
/// 3. If probe fails or produces too few → false positive, try next candidate
///
/// This avoids spending 50-500ms decoding from false positive positions.
fn scan_and_decode_chunk(
    data: &[u8],
    scan_start_byte: usize,
    stop_byte: usize,
    deflate_end: usize,
) -> Option<(usize, Vec<LZ77Token>)> {
    let scan_end_byte = (scan_start_byte + SCAN_WINDOW_BYTES).min(deflate_end);
    let stop_bit = stop_byte * 8;

    let mut search_from = scan_start_byte * 8;
    let search_end = scan_end_byte * 8;

    while search_from < search_end {
        let candidate = match scan_for_block(data, search_from, search_end) {
            Some(b) => b,
            None => break,
        };

        // Stage 1: Probe decode — only decode up to PROBE_BYTES
        let probe_stop = (candidate.bit_offset + PROBE_BYTES * 8).min(stop_bit);
        let probe_ok = match decode_chunk_tokens(data, candidate.bit_offset, probe_stop) {
            Ok(tokens) => tokens.len() >= MIN_PROBE_TOKENS,
            Err(_) => false,
        };

        if probe_ok {
            // Stage 2: Full decode from this boundary to chunk end
            match decode_chunk_tokens(data, candidate.bit_offset, stop_bit) {
                Ok(tokens) if !tokens.is_empty() => {
                    return Some((candidate.bit_offset, tokens));
                }
                _ => {}
            }
        }

        search_from = candidate.bit_offset + 1;
    }

    None
}

/// Phase 1: Find DEFLATE block boundaries for each chunk.
///
/// Thread 0 always starts at `header_size * 8` (the first DEFLATE block).
/// Threads K>0 scan for a valid block boundary near their chunk start.
/// If scanning fails, the boundary is set to `deflate_end * 8` so the predecessor
/// absorbs that chunk. Failed boundaries are filtered out before returning.
/// Decode DEFLATE blocks from `start_bit` to `stop_bit`, emitting raw LZ77 tokens.
///
/// This does NOT maintain a sliding window. Copy tokens are emitted as-is with their
/// length and distance values. The sequential BoundaryResolver in Phase 3 handles
/// all context resolution.
fn decode_chunk_tokens(data: &[u8], start_bit: usize, stop_bit: usize) -> Result<Vec<LZ77Token>> {
    let mut tokens = Vec::with_capacity(65536);
    let mut bits = SliceBitReader::new(data);

    let start_byte = start_bit / 8;
    let start_bit_offset = (start_bit % 8) as u8;
    bits.set_bit_position(start_byte, start_bit_offset);

    loop {
        // Check if we've reached or passed the stop position
        let (cur_byte, cur_bit) = bits.bit_position();
        let cur_abs_bit = cur_byte * 8 + cur_bit as usize;
        if cur_abs_bit >= stop_bit {
            break;
        }

        // Read BFINAL and BTYPE
        let bfinal = match bits.read_bits(1) {
            Ok(v) => v != 0,
            Err(_) => break,
        };
        let btype = match bits.read_bits(2) {
            Ok(v) => v,
            Err(_) => break,
        };

        match btype {
            0 => {
                // Stored block: align to byte, read LEN/NLEN, emit literals
                bits.align_to_byte();
                let len = match bits.read_u16_le() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let nlen = match bits.read_u16_le() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                if len != !nlen {
                    break; // Invalid stored block
                }
                let mut stored_complete = true;
                for _ in 0..len {
                    match bits.read_bits(8) {
                        Ok(b) => tokens.push(LZ77Token::Literal(b as u8)),
                        Err(_) => {
                            // Incomplete stored block — stop decoding entirely.
                            // Near chunk boundaries this is expected; the sequential
                            // resolver handles any truncation.
                            stored_complete = false;
                            break;
                        }
                    }
                }
                if !stored_complete {
                    return Ok(tokens);
                }
                tokens.push(LZ77Token::EndOfBlock);
            }
            1 => {
                // Fixed Huffman
                let lit_decoder = HuffmanDecoder::fixed_literal_length();
                let dist_decoder = HuffmanDecoder::fixed_distance();
                decode_huffman_block(&mut bits, &lit_decoder, Some(&dist_decoder), &mut tokens)?;
            }
            2 => {
                // Dynamic Huffman
                let (lit_decoder, dist_decoder) = parse_dynamic_huffman_tables(&mut bits)?;
                decode_huffman_block(&mut bits, &lit_decoder, dist_decoder.as_ref(), &mut tokens)?;
            }
            _ => break, // Reserved block type
        }

        if bfinal {
            break;
        }
    }

    Ok(tokens)
}

/// Decode Huffman symbols from a single DEFLATE block, emitting LZ77 tokens.
/// No sliding window is maintained.
fn decode_huffman_block(
    bits: &mut SliceBitReader<'_>,
    lit_decoder: &HuffmanDecoder,
    dist_decoder: Option<&HuffmanDecoder>,
    tokens: &mut Vec<LZ77Token>,
) -> Result<()> {
    loop {
        let sym = lit_decoder.decode(bits)?;

        if sym <= 255 {
            tokens.push(LZ77Token::Literal(sym as u8));
            continue;
        }

        if sym == 256 {
            tokens.push(LZ77Token::EndOfBlock);
            break;
        }

        // Length code (257..=285)
        if sym > 285 {
            return Err(Error::InvalidLengthCode(sym));
        }

        let len_idx = (sym - 257) as usize;
        let (base_len, extra_bits) = LENGTH_TABLE[len_idx];
        let extra = if extra_bits > 0 { bits.read_bits(extra_bits)? } else { 0 };
        let length = base_len + extra as u16;

        // Read distance
        let dist_dec = dist_decoder.ok_or(Error::InvalidDistanceCode(0))?;
        let dist_sym = dist_dec.decode(bits)?;
        if dist_sym > 29 {
            return Err(Error::InvalidDistanceCode(dist_sym));
        }

        let (base_dist, dist_extra_bits) = DISTANCE_TABLE[dist_sym as usize];
        let dist_extra = if dist_extra_bits > 0 { bits.read_bits(dist_extra_bits)? } else { 0 };
        let distance = base_dist + dist_extra as u16;

        tokens.push(LZ77Token::Copy { length, distance });
    }

    Ok(())
}

// --- Parallel encoding infrastructure ---

/// A job for a worker thread to encode a resolved BGZF block.
struct EncodingJob {
    block_id: u64,
    tokens: Vec<LZ77Token>,
    uncompressed_size: u32,
}

/// Result from a worker: an encoded BGZF block ready to write.
struct EncodedBlock {
    block_id: u64,
    data: Vec<u8>,
    uncompressed_size: u32,
}

/// Worker thread: receives resolved token blocks, computes CRC, encodes BGZF.
fn encoding_worker(
    job_rx: Receiver<EncodingJob>,
    result_tx: Sender<Result<EncodedBlock>>,
    use_fixed_huffman: bool,
) {
    let mut encoder = HuffmanEncoder::new(use_fixed_huffman);

    while let Ok(job) = job_rx.recv() {
        let uncompressed_bytes = tokens_to_bytes(&job.tokens);
        let crc = crc32fast::hash(&uncompressed_bytes);
        let isize = job.uncompressed_size;

        let result = match encoder.encode(&job.tokens, true) {
            Ok(deflate_data) => {
                let block_size = 18 + deflate_data.len() + 8;
                let bsize = block_size - 1;
                let mut data = Vec::with_capacity(block_size);

                data.extend_from_slice(&[
                    0x1f,
                    0x8b,
                    0x08,
                    0x04,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0xff,
                    0x06,
                    0x00,
                    0x42,
                    0x43,
                    0x02,
                    0x00,
                    (bsize & 0xFF) as u8,
                    ((bsize >> 8) & 0xFF) as u8,
                ]);
                data.extend_from_slice(&deflate_data);
                data.extend_from_slice(&crc.to_le_bytes());
                data.extend_from_slice(&isize.to_le_bytes());

                Ok(EncodedBlock { block_id: job.block_id, data, uncompressed_size: isize })
            }
            Err(e) => Err(e),
        };

        if result_tx.send(result).is_err() {
            break;
        }
    }
}

/// Send a job to workers, draining results if the channel is full (prevents deadlock).
#[allow(clippy::too_many_arguments)]
fn send_job_and_drain<W: Write>(
    job_tx: &Sender<EncodingJob>,
    result_rx: &Receiver<Result<EncodedBlock>>,
    job: EncodingJob,
    writer: &mut W,
    pending_blocks: &mut BTreeMap<u64, EncodedBlock>,
    next_write_id: &mut u64,
    blocks_written: &mut u64,
    output_bytes: &mut u64,
    build_index: bool,
    index_entries: &mut Vec<GziEntry>,
    current_compressed_offset: &mut u64,
    current_uncompressed_offset: &mut u64,
) -> Result<()> {
    let mut job_to_send = Some(job);
    while let Some(j) = job_to_send.take() {
        match job_tx.try_send(j) {
            Ok(()) => {}
            Err(crossbeam::channel::TrySendError::Full(returned)) => {
                job_to_send = Some(returned);
                match result_rx.recv() {
                    Ok(result) => {
                        let block = result?;
                        buffer_and_write_block(
                            writer,
                            block,
                            pending_blocks,
                            next_write_id,
                            blocks_written,
                            output_bytes,
                            build_index,
                            index_entries,
                            current_compressed_offset,
                            current_uncompressed_offset,
                        )?;
                    }
                    Err(_) => return Err(Error::Internal("Result channel disconnected".into())),
                }
            }
            Err(crossbeam::channel::TrySendError::Disconnected(_)) => {
                return Err(Error::Internal("Workers disconnected".into()));
            }
        }
    }
    Ok(())
}

/// Buffer an out-of-order block, writing consecutive blocks when possible.
#[allow(clippy::too_many_arguments)]
fn buffer_and_write_block<W: Write>(
    writer: &mut W,
    block: EncodedBlock,
    pending: &mut BTreeMap<u64, EncodedBlock>,
    next_write_id: &mut u64,
    blocks_written: &mut u64,
    output_bytes: &mut u64,
    build_index: bool,
    index_entries: &mut Vec<GziEntry>,
    current_compressed_offset: &mut u64,
    current_uncompressed_offset: &mut u64,
) -> Result<()> {
    if block.block_id == *next_write_id {
        write_single_block(
            writer,
            &block.data,
            block.uncompressed_size,
            output_bytes,
            build_index,
            index_entries,
            current_compressed_offset,
            current_uncompressed_offset,
        )?;
        *blocks_written += 1;
        *next_write_id += 1;

        while let Some(buffered) = pending.remove(next_write_id) {
            write_single_block(
                writer,
                &buffered.data,
                buffered.uncompressed_size,
                output_bytes,
                build_index,
                index_entries,
                current_compressed_offset,
                current_uncompressed_offset,
            )?;
            *blocks_written += 1;
            *next_write_id += 1;
        }
    } else {
        pending.insert(block.block_id, block);
    }
    Ok(())
}

/// Write one BGZF block to output and update tracking.
#[allow(clippy::too_many_arguments)]
fn write_single_block<W: Write>(
    writer: &mut W,
    data: &[u8],
    uncompressed_size: u32,
    output_bytes: &mut u64,
    build_index: bool,
    index_entries: &mut Vec<GziEntry>,
    current_compressed_offset: &mut u64,
    current_uncompressed_offset: &mut u64,
) -> Result<()> {
    if build_index {
        index_entries.push(GziEntry {
            compressed_offset: *current_compressed_offset,
            uncompressed_offset: *current_uncompressed_offset,
        });
    }
    *output_bytes += data.len() as u64;
    writer.write_all(data).map_err(Error::Io)?;
    *current_compressed_offset += data.len() as u64;
    *current_uncompressed_offset += uncompressed_size as u64;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fastq(num_reads: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(num_reads * 80);
        for i in 0..num_reads {
            buf.extend_from_slice(
                format!(
                    "@SEQ_{i}\n\
                     ACGTACGTACGTACGTACGTACGTACGTACGT\n\
                     +\n\
                     IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIII\n"
                )
                .as_bytes(),
            );
        }
        buf
    }

    fn gzip_compress(input: &[u8]) -> Vec<u8> {
        use std::io::Write as IoWrite;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(input).unwrap();
        enc.finish().unwrap()
    }

    fn gzip_decompress(data: &[u8]) -> Vec<u8> {
        use std::io::Read as IoRead;
        let mut decoder = flate2::read::MultiGzDecoder::new(data);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();
        output
    }

    #[test]
    fn test_parallel_roundtrip() {
        let original = make_fastq(10_000);
        let gz = gzip_compress(&original);

        let config = TranscodeConfig { num_threads: 4, ..Default::default() };
        let mut transcoder = ParallelDecodeTranscoder::new(config).with_min_region_bytes(0);

        let mut bgzf_output = Vec::new();
        let stats = transcoder.transcode_mmap(&gz, &mut bgzf_output).unwrap();

        assert!(stats.blocks_written >= 1);

        let decompressed = gzip_decompress(&bgzf_output);
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_parallel_matches_single_threaded() {
        let original = make_fastq(10_000);
        let gz = gzip_compress(&original);

        // Single-threaded
        let mut st = SingleThreadedTranscoder::new(TranscodeConfig::default());
        let mut st_output = Vec::new();
        st.transcode_slice(&gz, &mut st_output).unwrap();

        // Parallel (with min_region_bytes=0 to ensure parallel path is taken)
        let config = TranscodeConfig { num_threads: 4, ..Default::default() };
        let mut pd = ParallelDecodeTranscoder::new(config).with_min_region_bytes(0);
        let mut pd_output = Vec::new();
        pd.transcode_mmap(&gz, &mut pd_output).unwrap();

        let st_dec = gzip_decompress(&st_output);
        let pd_dec = gzip_decompress(&pd_output);
        assert_eq!(st_dec, pd_dec);
    }

    #[test]
    fn test_falls_back_for_small_input() {
        let original = make_fastq(10);
        let gz = gzip_compress(&original);

        let config = TranscodeConfig { num_threads: 4, ..Default::default() };
        let mut transcoder = ParallelDecodeTranscoder::new(config);

        let mut bgzf_output = Vec::new();
        let stats = transcoder.transcode_mmap(&gz, &mut bgzf_output).unwrap();

        assert!(stats.blocks_written >= 1);

        let decompressed = gzip_decompress(&bgzf_output);
        assert_eq!(decompressed, original);
    }
}
