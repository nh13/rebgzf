//! Shared parallel encoding infrastructure for BGZF block encoding.
//!
//! Contains the encoding job/result types, worker thread function, and
//! output ordering helpers used by both `parallel.rs` and `parallel_decode.rs`.

use std::collections::BTreeMap;
use std::io::Write;

use crossbeam::channel::{Receiver, Sender};

use crate::bgzf::GziEntry;
use crate::deflate::LZ77Token;
use crate::error::{Error, Result};
use crate::huffman::HuffmanEncoder;

/// A job for a worker thread to encode a resolved BGZF block.
pub(super) struct EncodingJob {
    pub block_id: u64,
    pub tokens: Vec<LZ77Token>,
    pub uncompressed_size: u32,
    pub crc: u32,
}

/// Result from a worker: an encoded BGZF block ready to write.
pub(super) struct EncodedBlock {
    pub block_id: u64,
    pub data: Vec<u8>,
    pub uncompressed_size: u32,
}

/// Encode a single BGZF block from resolved tokens.
pub(super) fn encode_block(encoder: &mut HuffmanEncoder, job: EncodingJob) -> Result<EncodedBlock> {
    let crc = job.crc;
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

    Ok(EncodedBlock { block_id: job.block_id, data, uncompressed_size: isize })
}

/// Worker thread: receives encoding jobs and sends back encoded BGZF blocks.
pub(super) fn encoding_worker(
    job_rx: Receiver<EncodingJob>,
    result_tx: Sender<Result<EncodedBlock>>,
    use_fixed_huffman: bool,
) {
    let mut encoder = HuffmanEncoder::new(use_fixed_huffman);
    while let Ok(job) = job_rx.recv() {
        let result = encode_block(&mut encoder, job);
        if result_tx.send(result).is_err() {
            break;
        }
    }
}

/// Send a job to workers, draining results if the channel is full (prevents deadlock).
#[allow(clippy::too_many_arguments)]
pub(super) fn send_job_and_drain<W: Write>(
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
pub(super) fn buffer_and_write_block<W: Write>(
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
pub(super) fn write_single_block<W: Write>(
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
