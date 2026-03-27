//! Chunk fetcher that orchestrates parallel speculative decode + marker replacement.
//!
//! Splits compressed DEFLATE data into chunks, decodes them in parallel using
//! [`speculative_decode`], and resolves markers sequentially once preceding
//! windows become available.

use super::chunk::ChunkData;
use super::replacer::replace_markers;
use super::speculative::{decode_with_libdeflate, speculative_decode};
use crate::error::{Error, Result};
use crate::gzip::GzipHeader;
use crate::transcoder::block_scanner::{scan_for_block, BlockBoundary};

/// Configuration for the parallel chunk fetcher.
pub struct FetcherConfig {
    /// Number of worker threads for parallel decode.
    pub threads: usize,
    /// Target bytes of compressed data per chunk (default 4 MiB).
    pub chunk_size: usize,
    /// Whether to verify CRC32 checksums after decompressing each member (default: true).
    pub verify_crc: bool,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        Self { threads: 4, chunk_size: 4 * 1024 * 1024, verify_crc: true }
    }
}

/// Parallel chunk fetcher that decodes and resolves DEFLATE chunks.
///
/// All chunks are decoded in parallel using [`crossbeam::scope`] and resolved
/// sequentially before being handed out via [`next_chunk`](Self::next_chunk).
pub struct ChunkFetcher {
    /// All chunks, fully resolved and in order.
    resolved_chunks: Vec<ChunkData>,
    /// Index of the next chunk to hand out.
    next_index: usize,
}

impl ChunkFetcher {
    /// Create a fetcher and run the parallel decode pipeline.
    ///
    /// This blocks until all chunks are decoded and resolved.
    ///
    /// # Arguments
    /// * `data` - the full compressed file data (e.g. mmap'd)
    /// * `header_size` - byte offset where DEFLATE data begins (after gzip header)
    /// * `deflate_end` - byte offset where DEFLATE data ends (before gzip trailer)
    /// * `config` - parallelism and chunking configuration
    pub fn new(
        data: &[u8],
        header_size: usize,
        deflate_end: usize,
        config: FetcherConfig,
    ) -> Result<Self> {
        let num_threads = config.threads.max(1);

        // Generate chunk boundaries (bit offsets).
        let boundaries = generate_boundaries(data, header_size, deflate_end, config.chunk_size);

        if boundaries.is_empty() {
            return Ok(Self { resolved_chunks: vec![], next_index: 0 });
        }

        if boundaries.len() == 1 {
            // Single chunk: use libdeflate fast path for the entire DEFLATE stream.
            let deflate_data = &data[header_size..deflate_end];

            // Read ISIZE from gzip trailer (last 4 bytes of trailer = expected decompressed size mod 2^32).
            let isize_bytes = &data[deflate_end..deflate_end + 4.min(data.len() - deflate_end)];
            let expected_size = if isize_bytes.len() >= 4 {
                u32::from_le_bytes([isize_bytes[0], isize_bytes[1], isize_bytes[2], isize_bytes[3]])
                    as usize
            } else {
                // Can't determine size — fall back to speculative decode.
                let (start_bit, end_bit) = boundaries[0];
                let chunk = speculative_decode(data, start_bit, end_bit, Some(&[]))?;
                return Ok(Self { resolved_chunks: vec![chunk], next_index: 0 });
            };

            match decode_with_libdeflate(deflate_data, expected_size) {
                Ok(decompressed) => {
                    let mut chunk = ChunkData::new(header_size * 8);
                    chunk.encoded_size = (deflate_end - header_size) * 8;
                    chunk.append_resolved(decompressed);
                    chunk.recompute_final_window();
                    return Ok(Self { resolved_chunks: vec![chunk], next_index: 0 });
                }
                Err(_) => {
                    // libdeflate failed (e.g. ISIZE overflow for >4GB files) — fall back.
                    let (start_bit, end_bit) = boundaries[0];
                    let chunk = speculative_decode(data, start_bit, end_bit, Some(&[]))?;
                    return Ok(Self { resolved_chunks: vec![chunk], next_index: 0 });
                }
            }
        }

        // Parallel decode all chunks using a bounded work queue.
        let num_chunks = boundaries.len();
        let mut decoded_chunks: Vec<Option<std::result::Result<ChunkData, Error>>> =
            (0..num_chunks).map(|_| None).collect();

        let scope_result = crossbeam::scope(|scope| {
            let (work_tx, work_rx) =
                crossbeam::channel::bounded::<(usize, usize, usize, Option<Vec<u8>>)>(num_chunks);
            let (result_tx, result_rx) = crossbeam::channel::bounded(num_threads * 2);

            // Enqueue all work items.
            for (idx, &(start_bit, end_bit)) in boundaries.iter().enumerate() {
                let initial_window = if idx == 0 { Some(vec![]) } else { None };
                work_tx.send((idx, start_bit, end_bit, initial_window)).unwrap();
            }
            drop(work_tx);

            // Spawn exactly num_threads workers that pull from the work queue.
            for _ in 0..num_threads {
                let work_rx = work_rx.clone();
                let result_tx = result_tx.clone();
                scope.spawn(move |_| {
                    while let Ok((idx, start_bit, end_bit, initial_window)) = work_rx.recv() {
                        let window_ref = initial_window.as_deref();
                        let result = speculative_decode(data, start_bit, end_bit, window_ref);
                        let _ = result_tx.send((idx, result));
                    }
                });
            }
            drop(result_tx);

            // Collect results.
            for (idx, result) in result_rx {
                decoded_chunks[idx] = Some(result);
            }
        });

        // Handle crossbeam scope panics.
        if let Err(e) = scope_result {
            return Err(Error::Internal(format!("worker thread panicked: {e:?}")));
        }

        // Unwrap results, propagating the first error.
        let mut chunks: Vec<ChunkData> = Vec::with_capacity(num_chunks);
        for (i, slot) in decoded_chunks.into_iter().enumerate() {
            match slot {
                Some(Ok(chunk)) => chunks.push(chunk),
                Some(Err(e)) => return Err(e),
                None => {
                    return Err(Error::Internal(format!("chunk {i} was not produced by workers")));
                }
            }
        }

        // Ensure chunk 0's final_window is correct after parallel decode.
        if !chunks.is_empty() && chunks[0].is_resolved() {
            chunks[0].recompute_final_window();
        }

        // Sequential marker resolution: resolve chunk K+1 using chunk K's final window.
        for i in 1..chunks.len() {
            let prev_window = chunks[i - 1]
                .final_window
                .clone()
                .expect("speculative_decode must set final_window");
            if !chunks[i].is_resolved() {
                replace_markers(&mut chunks[i], &prev_window, false);
            }
        }

        Ok(Self { resolved_chunks: chunks, next_index: 0 })
    }

    /// Create a fetcher from raw file data, automatically detecting multi-member gzip.
    ///
    /// For single-member files, uses the speculative parallel decode path.
    /// Multi-member files should use [`decode_member_batch`] via the reader's
    /// lazy batch decoding instead of pre-decoding everything here.
    pub fn from_data(data: &[u8], config: FetcherConfig) -> Result<Self> {
        let members = scan_gzip_members(data);
        match members.as_slice() {
            [] => Err(Error::Internal("no gzip members found".into())),
            [member] => Self::from_member(data, *member, config),
            _ => Err(Error::Internal(
                "multiple gzip members detected; use decode_member_batch for concatenated streams"
                    .into(),
            )),
        }
    }

    /// Create a fetcher for a single gzip member with known byte boundaries.
    ///
    /// Use this when member boundaries have already been scanned (e.g. via
    /// [`scan_gzip_members`]) to avoid a redundant scan.
    pub fn from_member(data: &[u8], member: (usize, usize), config: FetcherConfig) -> Result<Self> {
        let (start, end) = member;
        if start >= end || end > data.len() {
            return Err(Error::Internal(format!(
                "invalid member range ({start}, {end}) for input of {} bytes",
                data.len()
            )));
        }
        let member_data = &data[start..end];
        let mut cursor = std::io::Cursor::new(member_data);
        let _header = GzipHeader::parse(&mut cursor)
            .map_err(|e| Error::Internal(format!("gzip header: {e}")))?;
        let header_len = cursor.position() as usize;
        if member_data.len() < header_len + 8 {
            return Err(Error::Internal(format!("member range ({start}, {end}) is truncated")));
        }
        let header_size = start + header_len;

        // Trailer is last 8 bytes of the member.
        let deflate_end = end - 8;

        Self::new(data, header_size, deflate_end, config)
    }

    /// Get the next resolved chunk, or `None` if all consumed.
    pub fn next_chunk(&mut self) -> Option<ChunkData> {
        if self.next_index < self.resolved_chunks.len() {
            let chunk =
                std::mem::replace(&mut self.resolved_chunks[self.next_index], ChunkData::new(0));
            self.next_index += 1;
            Some(chunk)
        } else {
            None
        }
    }
}

/// Check whether the bytes at `pos` in `data` look like a valid gzip member header.
///
/// Validates magic bytes (`0x1f 0x8b`), DEFLATE method (`0x08`), reserved flag
/// bits (bits 5-7 must be zero), and attempts to parse the full header via
/// [`GzipHeader::parse`].
fn is_gzip_header(data: &[u8], pos: usize) -> bool {
    if pos + 10 > data.len() {
        return false;
    }
    // Magic + method
    if data[pos] != 0x1f || data[pos + 1] != 0x8b || data[pos + 2] != 0x08 {
        return false;
    }
    // Flags: bits 5-7 are reserved and must be zero.
    let flags = data[pos + 3];
    if flags & 0xE0 != 0 {
        return false;
    }
    // Try to parse the full header (validates extra, filename, comment, CRC fields).
    let mut cursor = std::io::Cursor::new(&data[pos..]);
    GzipHeader::parse(&mut cursor).is_ok()
}

/// Scan file data for gzip member boundaries (heuristic).
///
/// Returns a list of `(member_start, member_end)` byte offset pairs.
/// Each member spans from its gzip header to the byte before the next member
/// (or EOF).
///
/// # Heuristic behaviour
///
/// This scanner uses two strategies to locate member boundaries and may produce
/// **false positives** (byte sequences that look like gzip headers but are not)
/// or **false negatives** (members with empty payloads or `ISIZE > 1 MiB`).
/// For uniform-header files produced by `pigz` or `bgzip` the scan is exact.
///
/// When consuming the returned boundaries, use [`decode_member_batch`] which
/// contains a merge-retry path that handles false-positive boundaries
/// gracefully, or validate each boundary independently before use.
///
/// # Strategy
///
/// First try exact-match scanning using the first member's complete
/// header bytes as a signature (fast, no false positives for pigz/bgzip where
/// all members share identical headers). If the first exact-match scan finds
/// only one member but the file is large, fall back to structural validation
/// to handle heterogeneous headers.
pub fn scan_gzip_members(data: &[u8]) -> Vec<(usize, usize)> {
    if data.len() < 10 {
        return vec![];
    }

    // Validate the first member.
    if !is_gzip_header(data, 0) {
        return vec![];
    }

    // Parse first header to get its exact length.
    let mut cursor = std::io::Cursor::new(data);
    let _header = match GzipHeader::parse(&mut cursor) {
        Ok(h) => h,
        Err(_) => return vec![],
    };
    let first_header_len = cursor.position() as usize;
    let sig = &data[..first_header_len];

    // Phase 1: exact-signature scan (fast, zero false positives for uniform headers).
    let exact_members = scan_with_pattern(data, sig, first_header_len);

    if exact_members.len() > 1 {
        return exact_members;
    }

    // Phase 2: structural scan for heterogeneous members (different flags/timestamps).
    let structural_members = scan_with_validator(data, first_header_len);

    if structural_members.len() > exact_members.len() {
        return structural_members;
    }

    exact_members
}

/// Scan for members using an exact byte pattern for the header.
fn scan_with_pattern(data: &[u8], sig: &[u8], first_header_len: usize) -> Vec<(usize, usize)> {
    let sig_len = sig.len();
    let mut members = Vec::new();
    let mut pos = 0;

    loop {
        let min_end = pos + first_header_len.max(10) + 8;
        let search_start = min_end.min(data.len());

        if search_start + sig_len > data.len() {
            members.push((pos, data.len()));
            break;
        }

        let search_region = &data[search_start..];
        let mut search_offset = 0;
        let mut next_member = data.len();

        while let Some(found) = memchr::memchr(sig[0], &search_region[search_offset..]) {
            let abs_pos = search_start + search_offset + found;
            if abs_pos + sig_len <= data.len() && data[abs_pos..abs_pos + sig_len] == *sig {
                next_member = abs_pos;
                break;
            }
            search_offset += found + 1;
        }

        members.push((pos, next_member));
        pos = next_member;

        if next_member >= data.len() {
            break;
        }
    }

    members
}

/// Scan for members using structural validation (for heterogeneous headers).
///
/// This is the Phase 2 fallback scanner, used when the exact-pattern scanner
/// (Phase 1) fails to find members.  It validates gzip magic bytes, DEFLATE
/// method, reserved flags, and header parseability at each candidate position.
///
/// As a heuristic to reduce false positives, the preceding trailer's ISIZE
/// field is required to be nonzero and <= 1 MiB.  This means members with an
/// empty decompressed payload (`ISIZE == 0`) or very large decompressed sizes
/// (`ISIZE > 1 MiB`, noting that ISIZE wraps at 4 GiB) will not be detected
/// as boundaries by this scanner.  Uniform-header files (pigz, bgzip) are
/// handled by the Phase 1 exact-pattern scanner where this limitation does not
/// apply.  Any false-positive boundaries that slip through are caught by the
/// merge-retry logic in [`decode_member_batch`].
fn scan_with_validator(data: &[u8], first_header_len: usize) -> Vec<(usize, usize)> {
    let mut members = Vec::new();
    let mut pos = 0;

    loop {
        let min_end = pos + first_header_len.max(10) + 8;
        let search_start = min_end.min(data.len());

        if search_start + 10 > data.len() {
            members.push((pos, data.len()));
            break;
        }

        let search_region = &data[search_start..];
        let mut search_offset = 0;
        let mut next_member = data.len();

        while let Some(found) = memchr::memchr(0x1f, &search_region[search_offset..]) {
            let abs_pos = search_start + search_offset + found;

            if is_gzip_header(data, abs_pos) && abs_pos >= pos + 18 {
                // Additional validation: check that the ISIZE in the preceding
                // trailer is plausible (nonzero and <= 1 MiB for typical members).
                if abs_pos >= 4 {
                    let isize_bytes = &data[abs_pos - 4..abs_pos];
                    let isize_val = u32::from_le_bytes([
                        isize_bytes[0],
                        isize_bytes[1],
                        isize_bytes[2],
                        isize_bytes[3],
                    ]);
                    if isize_val > 0 && isize_val <= 1_048_576 {
                        next_member = abs_pos;
                        break;
                    }
                }
            }
            search_offset += found + 1;
        }

        members.push((pos, next_member));
        pos = next_member;

        if next_member >= data.len() {
            break;
        }
    }

    members
}

/// Decompress a batch of gzip members in parallel using libdeflate.
///
/// Each member is independent (fresh DEFLATE window), so they can be decoded
/// concurrently without inter-member dependencies. Returns one [`ChunkData`]
/// per member, in order.
///
/// If a candidate member fails to decompress (false-positive boundary), it is
/// merged with the previous member and retried.
pub fn decode_member_batch(
    data: &[u8],
    members: &[(usize, usize)],
    num_threads: usize,
    verify_crc: bool,
) -> Result<Vec<ChunkData>> {
    let num_threads = num_threads.max(1);
    let num_members = members.len();

    if num_members == 0 {
        return Ok(vec![]);
    }

    let mut decoded_chunks: Vec<Option<std::result::Result<ChunkData, Error>>> =
        (0..num_members).map(|_| None).collect();

    let scope_result = crossbeam::scope(|scope| {
        let (work_tx, work_rx) = crossbeam::channel::bounded::<(usize, usize, usize)>(num_members);
        let (result_tx, result_rx) = crossbeam::channel::bounded(num_threads * 2);

        // Enqueue all members.
        for (idx, &(start, end)) in members.iter().enumerate() {
            work_tx.send((idx, start, end)).unwrap();
        }
        drop(work_tx);

        // Spawn worker threads.
        for _ in 0..num_threads {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move |_| {
                let mut decompressor = libdeflater::Decompressor::new();
                while let Ok((idx, start, end)) = work_rx.recv() {
                    let result =
                        decode_single_member(data, start, end, &mut decompressor, verify_crc);
                    let _ = result_tx.send((idx, result));
                }
            });
        }
        drop(result_tx);

        // Collect results.
        for (idx, result) in result_rx {
            decoded_chunks[idx] = Some(result);
        }
    });

    if let Err(e) = scope_result {
        return Err(Error::Internal(format!("worker thread panicked: {e:?}")));
    }

    // Unwrap results in order, merging failed members with the previous member
    // (false-positive boundary detection).
    let mut chunks: Vec<ChunkData> = Vec::with_capacity(num_members);

    for (i, slot) in decoded_chunks.into_iter().enumerate() {
        match slot {
            Some(Ok(chunk)) => {
                chunks.push(chunk);
            }
            Some(Err(_e)) => {
                // This member failed to decompress — likely a false-positive boundary.
                // Merge it with the previous member by extending the previous member's
                // range and re-decoding.
                if chunks.is_empty() {
                    // First member failed — cannot merge.
                    return Err(_e);
                }
                let (_failed_start, failed_end) = members[i];
                let prev_offset = chunks.last().unwrap().encoded_offset / 8;
                let mut decompressor = libdeflater::Decompressor::new();
                match decode_single_member(
                    data,
                    prev_offset,
                    failed_end,
                    &mut decompressor,
                    verify_crc,
                ) {
                    Ok(merged) => {
                        *chunks.last_mut().unwrap() = merged;
                    }
                    Err(e2) => return Err(e2),
                }
            }
            None => {
                return Err(Error::Internal(format!("member {i} was not produced by workers")));
            }
        }
    }

    Ok(chunks)
}

/// Decompress a single gzip member using libdeflate.
///
/// Parses the gzip header, extracts DEFLATE data between the header and 8-byte
/// trailer, reads ISIZE from the trailer, decompresses with libdeflate, and
/// optionally verifies the CRC32 checksum.
fn decode_single_member(
    data: &[u8],
    start: usize,
    end: usize,
    decompressor: &mut libdeflater::Decompressor,
    verify_crc: bool,
) -> Result<ChunkData> {
    let member_data = &data[start..end];

    // Parse header.
    let mut cursor = std::io::Cursor::new(member_data);
    let _header = GzipHeader::parse(&mut cursor)
        .map_err(|e| Error::Internal(format!("member at {start} header: {e}")))?;
    let header_len = cursor.position() as usize;

    // DEFLATE data is between header and 8-byte trailer (CRC32 + ISIZE).
    if member_data.len() < header_len + 8 {
        return Err(Error::Internal(format!(
            "member at {start} too short: {} bytes, need at least {}",
            member_data.len(),
            header_len + 8
        )));
    }
    let deflate_data = &member_data[header_len..member_data.len() - 8];

    // Parse trailer: first 4 bytes = CRC32, last 4 bytes = ISIZE.
    let trailer = &member_data[member_data.len() - 8..];
    let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    // ISIZE is the original size mod 2^32, so it wraps for files >4 GiB.
    // An incorrect size hint causes libdeflate to fail with a size mismatch,
    // which is surfaced as an error to the caller.
    let isize_val = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;

    // Decompress with libdeflate.
    let mut output = vec![0u8; isize_val];
    let actual = decompressor
        .deflate_decompress(deflate_data, &mut output)
        .map_err(|e| Error::Internal(format!("member at {start} decompress: {e}")))?;
    output.truncate(actual);

    // Verify CRC32 if requested.
    if verify_crc {
        let computed_crc = crc32fast::hash(&output);
        if computed_crc != stored_crc {
            return Err(Error::Crc32Mismatch { expected: stored_crc, found: computed_crc });
        }
    }

    let mut chunk = ChunkData::new(start * 8);
    chunk.encoded_size = (end - start) * 8;
    chunk.append_resolved(output);
    chunk.recompute_final_window();
    Ok(chunk)
}

/// Generate chunk boundaries as `(start_bit, end_bit)` pairs.
///
/// Each boundary is a valid DEFLATE block start found by the block scanner.
/// The first chunk starts at the known DEFLATE beginning (after gzip header)
/// and subsequent splits are found by scanning near each `chunk_size` interval.
fn generate_boundaries(
    data: &[u8],
    header_size: usize,
    deflate_end: usize,
    chunk_size: usize,
) -> Vec<(usize, usize)> {
    let start_bit = header_size * 8;
    let end_bit = deflate_end * 8;

    if end_bit <= start_bit {
        return vec![];
    }

    let total_bytes = deflate_end - header_size;
    if total_bytes <= chunk_size {
        return vec![(start_bit, end_bit)];
    }

    let mut boundaries = vec![];
    let mut current_start = start_bit;
    let mut byte_offset = header_size;

    loop {
        let next_byte = byte_offset + chunk_size;

        if next_byte >= deflate_end {
            // Last chunk: extends to the end of DEFLATE data.
            boundaries.push((current_start, end_bit));
            break;
        }

        // Scan up to 1 MiB past the target split point for a valid block boundary.
        let scan_start_bit = next_byte * 8;
        let scan_limit = (next_byte + 1024 * 1024).min(deflate_end);
        let scan_end_bit = scan_limit * 8;

        match scan_for_block(data, scan_start_bit, scan_end_bit) {
            Some(BlockBoundary { bit_offset }) => {
                boundaries.push((current_start, bit_offset));
                current_start = bit_offset;
                byte_offset = bit_offset / 8;
            }
            None => {
                // No boundary found in the scan range. Extend the search window
                // by advancing one more chunk_size and trying again.
                byte_offset = scan_limit;
                if byte_offset >= deflate_end {
                    boundaries.push((current_start, end_bit));
                    break;
                }
            }
        }
    }

    // Safety: ensure the last boundary reaches the end.
    if let Some(last) = boundaries.last_mut() {
        if last.1 < end_bit {
            last.1 = end_bit;
        }
    }

    boundaries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip_compress(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn parse_header_size(gzip_data: &[u8]) -> usize {
        use crate::gzip::GzipHeader;
        let mut cursor = std::io::Cursor::new(gzip_data);
        let _header = GzipHeader::parse(&mut cursor).unwrap();
        cursor.position() as usize
    }

    #[test]
    fn test_single_chunk_small_data() {
        let original = b"Hello, world!";
        let compressed = gzip_compress(original);
        let header_size = parse_header_size(&compressed);
        let deflate_end = compressed.len().saturating_sub(8);
        let config = FetcherConfig { threads: 2, chunk_size: 4 * 1024 * 1024, verify_crc: true };
        let mut fetcher = ChunkFetcher::new(&compressed, header_size, deflate_end, config).unwrap();

        let chunk = fetcher.next_chunk().expect("should have one chunk");
        assert!(chunk.is_resolved());

        let output: Vec<u8> =
            chunk.resolved_data().iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(output, original.as_slice());
        assert!(fetcher.next_chunk().is_none());
    }

    #[test]
    fn test_scan_single_member() {
        let compressed = gzip_compress(b"hello");
        let members = scan_gzip_members(&compressed);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], (0, compressed.len()));
    }

    #[test]
    fn test_scan_multi_member() {
        let mut data = gzip_compress(b"first");
        let first_len = data.len();
        data.extend_from_slice(&gzip_compress(b"second"));
        let members = scan_gzip_members(&data);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], (0, first_len));
        assert_eq!(members[1], (first_len, data.len()));
    }

    #[test]
    fn test_scan_empty() {
        let members = scan_gzip_members(&[]);
        assert!(members.is_empty());
    }

    #[test]
    fn test_from_data_single_member() {
        let original = b"Hello from from_data!";
        let compressed = gzip_compress(original);
        let config = FetcherConfig { threads: 2, chunk_size: 4 * 1024 * 1024, verify_crc: true };
        let mut fetcher = ChunkFetcher::from_data(&compressed, config).unwrap();
        let chunk = fetcher.next_chunk().expect("should have one chunk");
        let output: Vec<u8> =
            chunk.resolved_data().iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_decode_member_batch() {
        let mut data = gzip_compress(b"alpha");
        data.extend_from_slice(&gzip_compress(b"beta"));
        data.extend_from_slice(&gzip_compress(b"gamma"));
        let members = scan_gzip_members(&data);
        assert_eq!(members.len(), 3);

        let chunks = decode_member_batch(&data, &members, 2, true).unwrap();
        assert_eq!(chunks.len(), 3);

        let mut output = Vec::new();
        for chunk in &chunks {
            for buf in chunk.resolved_data() {
                output.extend_from_slice(buf);
            }
        }
        assert_eq!(output, b"alphabetagamma");
    }

    #[test]
    fn test_decode_member_batch_crc_verification() {
        let data = gzip_compress(b"hello crc");
        let members = scan_gzip_members(&data);
        assert_eq!(members.len(), 1);

        // With verification enabled — should succeed.
        let chunks = decode_member_batch(&data, &members, 1, true).unwrap();
        assert_eq!(chunks.len(), 1);

        // Corrupt the CRC32 in the trailer (bytes at end-8..end-4).
        let mut corrupted = data.clone();
        let crc_pos = corrupted.len() - 8;
        corrupted[crc_pos] ^= 0xFF;

        // With verification enabled — should fail.
        let result = decode_member_batch(&corrupted, &[(0, corrupted.len())], 1, true);
        assert!(result.is_err());

        // With verification disabled — should succeed despite corruption.
        let result = decode_member_batch(&corrupted, &[(0, corrupted.len())], 1, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_scan_heterogeneous_members() {
        // Create members with different compression levels (different XFL byte).
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut member1 = Vec::new();
        {
            let mut e = GzEncoder::new(&mut member1, Compression::fast());
            e.write_all(b"fast member").unwrap();
            e.finish().unwrap();
        }
        let mut member2 = Vec::new();
        {
            let mut e = GzEncoder::new(&mut member2, Compression::best());
            e.write_all(b"best member").unwrap();
            e.finish().unwrap();
        }

        let mut data = member1.clone();
        let first_len = data.len();
        data.extend_from_slice(&member2);

        let members = scan_gzip_members(&data);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], (0, first_len));
        assert_eq!(members[1], (first_len, data.len()));
    }

    #[test]
    fn test_empty_boundaries() {
        let boundaries = generate_boundaries(&[], 0, 0, 1024);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn test_small_data_single_boundary() {
        let original = b"small";
        let compressed = gzip_compress(original);
        let header_size = parse_header_size(&compressed);
        let deflate_end = compressed.len().saturating_sub(8);

        let boundaries =
            generate_boundaries(&compressed, header_size, deflate_end, 4 * 1024 * 1024);
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].0, header_size * 8);
        assert_eq!(boundaries[0].1, deflate_end * 8);
    }
}
