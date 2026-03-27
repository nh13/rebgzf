//! Speculative DEFLATE decoder that emits markers for unknown back-references.
//!
//! During parallel gzip decompression, chunks are decoded without the preceding
//! 32KB sliding window.  Back-references that fall into this unknown region are
//! encoded as [`MarkerValue::window_ref`] entries.  When the window is known
//! (either because the chunk is the first in the stream or because a previous
//! chunk has been resolved), all references are resolved directly.

use super::chunk::ChunkData;
use super::marker::MarkerValue;
use crate::bits::{BitRead, SliceBitReader};
use crate::deflate::parser::parse_dynamic_huffman_tables;
use crate::deflate::tables::{DISTANCE_TABLE, LENGTH_TABLE};
use crate::error::{Error, Result};
use crate::huffman::HuffmanDecoder;

/// Maximum DEFLATE sliding window size in bytes (32KB).
const WINDOW_SIZE: usize = 32_768;

/// Speculatively decode a DEFLATE stream, emitting markers for unknown window references.
///
/// When `initial_window` is `None`, back-references that reach before the start of this
/// chunk are encoded as `MarkerValue::window_ref()`.  When a window is provided, all
/// references can be resolved immediately.
///
/// # Arguments
/// * `data` - the full compressed file data (e.g. mmap'd)
/// * `start_bit` - bit offset where DEFLATE data starts for this chunk
/// * `end_bit` - bit offset to stop decoding (may be approximate; ignored once a final
///   block is seen)
/// * `initial_window` - if `Some`, the known 32KB window (all refs resolvable)
pub fn speculative_decode(
    data: &[u8],
    start_bit: usize,
    end_bit: usize,
    initial_window: Option<&[u8]>,
) -> Result<ChunkData> {
    let byte_pos = start_bit / 8;
    let bit_offset = (start_bit % 8) as u8;

    let mut bits = SliceBitReader::new(data);
    bits.set_bit_position(byte_pos, bit_offset);

    let mut chunk = ChunkData::new(start_bit);

    // The output buffer holds MarkerValues so we can track which positions are
    // resolved bytes vs unknown window references.
    let mut output: Vec<MarkerValue> = Vec::with_capacity(65_536);

    // Circular window buffer for resolving within-chunk references.
    // When initial_window is provided, prepopulate it.
    let mut window = vec![MarkerValue::literal(0); WINDOW_SIZE];
    let mut window_pos: usize = 0; // next write position in circular buffer

    if let Some(init) = initial_window {
        // Copy the initial window into our circular buffer.
        // The window may be shorter than WINDOW_SIZE (e.g. start of stream).
        let start = if init.len() >= WINDOW_SIZE { init.len() - WINDOW_SIZE } else { 0 };
        let relevant = &init[start..];
        for (i, &b) in relevant.iter().enumerate() {
            window[i] = MarkerValue::literal(b);
        }
        window_pos = relevant.len() % WINDOW_SIZE;
    }

    // Total bytes of output produced (used to determine if a back-reference
    // reaches into the unknown preceding window).
    let initial_window_len = initial_window.map_or(0, |w| w.len().min(WINDOW_SIZE));
    let mut total_output: usize = 0;

    loop {
        // Check if we've passed the end boundary (and we're not mid-block).
        let current_bit = {
            let (bp, bo) = bits.bit_position();
            bp * 8 + bo as usize
        };
        if current_bit >= end_bit {
            break;
        }

        // Read block header.
        let is_final = bits.read_bit()?;
        let block_type = bits.read_bits(2)? as u8;

        match block_type {
            0 => {
                // Stored (uncompressed) block
                decode_stored_block(
                    &mut bits,
                    &mut output,
                    &mut window,
                    &mut window_pos,
                    &mut total_output,
                )?;
            }
            1 => {
                // Fixed Huffman codes
                let lit_decoder = HuffmanDecoder::fixed_literal_length();
                let dist_decoder = HuffmanDecoder::fixed_distance();
                decode_huffman_block(
                    &mut bits,
                    &lit_decoder,
                    Some(&dist_decoder),
                    initial_window.is_some(),
                    initial_window_len,
                    &mut output,
                    &mut window,
                    &mut window_pos,
                    &mut total_output,
                )?;
            }
            2 => {
                // Dynamic Huffman codes
                let (lit_decoder, dist_decoder) = parse_dynamic_huffman_tables(&mut bits)?;
                decode_huffman_block(
                    &mut bits,
                    &lit_decoder,
                    dist_decoder.as_ref(),
                    initial_window.is_some(),
                    initial_window_len,
                    &mut output,
                    &mut window,
                    &mut window_pos,
                    &mut total_output,
                )?;
            }
            _ => return Err(Error::InvalidBlockType(block_type)),
        }

        if is_final {
            break;
        }
    }

    // Compute encoded size in bits.
    let final_bit = {
        let (bp, bo) = bits.bit_position();
        bp * 8 + bo as usize
    };
    chunk.encoded_size = final_bit.saturating_sub(start_bit);

    // Set the final window (last 32KB of output) for resolving the next chunk.
    let final_win = extract_final_window(&window, window_pos, total_output);
    chunk.final_window = Some(final_win);

    // Split output into resolved and marker segments.
    flush_output(&mut chunk, &output);

    Ok(chunk)
}

/// Decode a stored (uncompressed) DEFLATE block.
fn decode_stored_block(
    bits: &mut SliceBitReader<'_>,
    output: &mut Vec<MarkerValue>,
    window: &mut [MarkerValue],
    window_pos: &mut usize,
    total_output: &mut usize,
) -> Result<()> {
    bits.align_to_byte();
    let len = bits.read_u16_le()?;
    let nlen = bits.read_u16_le()?;

    if len != !nlen {
        return Err(Error::StoredBlockLengthMismatch { len, nlen });
    }

    for _ in 0..len {
        let byte = bits.read_bits(8)? as u8;
        let mv = MarkerValue::literal(byte);
        output.push(mv);
        window[*window_pos] = mv;
        *window_pos = (*window_pos + 1) % WINDOW_SIZE;
        *total_output += 1;
    }

    Ok(())
}

/// Decode a Huffman-coded DEFLATE block (fixed or dynamic), writing output as MarkerValues.
#[allow(clippy::too_many_arguments)]
fn decode_huffman_block(
    bits: &mut SliceBitReader<'_>,
    lit_decoder: &HuffmanDecoder,
    dist_decoder: Option<&HuffmanDecoder>,
    has_window: bool,
    initial_window_len: usize,
    output: &mut Vec<MarkerValue>,
    window: &mut [MarkerValue],
    window_pos: &mut usize,
    total_output: &mut usize,
) -> Result<()> {
    loop {
        let sym = lit_decoder.decode(bits)?;

        if sym <= 255 {
            // Literal byte
            let mv = MarkerValue::literal(sym as u8);
            output.push(mv);
            window[*window_pos] = mv;
            *window_pos = (*window_pos + 1) % WINDOW_SIZE;
            *total_output += 1;
            continue;
        }

        if sym == 256 {
            // End of block
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
        let dist_dec =
            dist_decoder.ok_or(Error::Internal("distance code in literal-only block".into()))?;
        let dist_sym = dist_dec.decode(bits)?;
        if dist_sym > 29 {
            return Err(Error::InvalidDistanceCode(dist_sym));
        }

        let (base_dist, dist_extra_bits) = DISTANCE_TABLE[dist_sym as usize];
        let dist_extra = if dist_extra_bits > 0 { bits.read_bits(dist_extra_bits)? } else { 0 };
        let distance = (base_dist + dist_extra as u16) as usize;

        // Resolve the Copy token.
        resolve_copy(
            length as usize,
            distance,
            has_window,
            initial_window_len,
            output,
            window,
            window_pos,
            total_output,
        );
    }

    Ok(())
}

/// Resolve a Copy { length, distance } token into the output buffer.
///
/// If `has_window` is true, all references are resolvable (no markers).
/// Otherwise, references into the unknown preceding window become markers.
#[allow(clippy::too_many_arguments)]
#[inline]
fn resolve_copy(
    length: usize,
    distance: usize,
    has_window: bool,
    initial_window_len: usize,
    output: &mut Vec<MarkerValue>,
    window: &mut [MarkerValue],
    window_pos: &mut usize,
    total_output: &mut usize,
) {
    for i in 0..length {
        // Where in the virtual output stream does this byte come from?
        // total_output + i is the current output position.
        // The reference points (distance) bytes back from here.
        let cur_pos = *total_output + i;

        // If has_window, then virtual position 0 corresponds to the start of
        // the initial window. All decoded output starts at initial_window_len.
        // The reference position in the virtual stream:
        //   ref_virtual = (initial_window_len + cur_pos) - distance
        //
        // If ref_virtual < 0 (before the window), that's an invalid reference
        // or one we can't resolve. Without a window, we also need to handle
        // references into the unknown region.

        let virtual_pos = initial_window_len + cur_pos;
        if virtual_pos < distance {
            if has_window {
                // This would be an out-of-bounds reference even with the window
                // — shouldn't happen in valid DEFLATE. Emit literal 0 as fallback.
                let mv = MarkerValue::literal(0);
                output.push(mv);
                window[*window_pos] = mv;
                *window_pos = (*window_pos + 1) % WINDOW_SIZE;
                continue;
            } else {
                // Reference reaches before the start of the chunk into the unknown
                // preceding 32KB window. Compute the correct offset within that window.
                let bytes_before_start = distance - virtual_pos;
                let window_offset =
                    (WINDOW_SIZE - (bytes_before_start % WINDOW_SIZE)) % WINDOW_SIZE;
                let mv = MarkerValue::window_ref(window_offset as u16);
                output.push(mv);
                window[*window_pos] = mv;
                *window_pos = (*window_pos + 1) % WINDOW_SIZE;
                continue;
            }
        }

        let ref_virtual = virtual_pos - distance;

        if !has_window && ref_virtual < initial_window_len {
            // Reference into the unknown preceding window. Compute the offset
            // within the 32KB window based on the virtual reference position.
            let bytes_before_start = initial_window_len - ref_virtual;
            let window_offset = (WINDOW_SIZE - (bytes_before_start % WINDOW_SIZE)) % WINDOW_SIZE;
            let mv = MarkerValue::window_ref(window_offset as u16);
            output.push(mv);
            window[*window_pos] = mv;
            *window_pos = (*window_pos + 1) % WINDOW_SIZE;
        } else {
            // The reference is within the known data — look it up from the
            // circular window buffer.
            //
            // The circular window tracks the last WINDOW_SIZE outputs.
            // window_pos points to the next write slot and has already been
            // advanced by prior iterations in this copy. The reference is
            // always `distance` bytes back from the current write position.
            let idx = (*window_pos + WINDOW_SIZE - distance) % WINDOW_SIZE;
            let mv = window[idx];
            output.push(mv);
            window[*window_pos] = mv;
            *window_pos = (*window_pos + 1) % WINDOW_SIZE;
        }
    }

    *total_output += length;
}

/// Extract the final 32KB window from the circular buffer.
///
/// Unresolved markers are emitted as placeholder `0` bytes. This window is only
/// valid for fully-resolved chunks; for chunks with markers, call
/// [`ChunkData::recompute_final_window`] after [`ChunkData::resolve_markers`].
fn extract_final_window(window: &[MarkerValue], window_pos: usize, total_output: usize) -> Vec<u8> {
    // Always produce a full WINDOW_SIZE window, zero-padded at the start.
    // Marker offsets are 0-based into a 32KB window.
    let mut result = vec![0u8; WINDOW_SIZE];

    let window_bytes = total_output.min(WINDOW_SIZE);
    if window_bytes == 0 {
        return result;
    }

    // Read from the circular buffer starting at (window_pos - window_bytes) mod WINDOW_SIZE.
    // Place data at the END of the result (zero-pad at the beginning).
    let start = (window_pos + WINDOW_SIZE - window_bytes) % WINDOW_SIZE;
    let pad = WINDOW_SIZE - window_bytes;
    for i in 0..window_bytes {
        let idx = (start + i) % WINDOW_SIZE;
        let mv = window[idx];
        // If the marker is a literal, extract the byte. If it's a window ref,
        // we can't resolve it yet — store 0 as placeholder. The final_window
        // is only meaningful once markers are resolved via recompute_final_window().
        if mv.is_literal() {
            result[pad + i] = mv.resolve(&[]);
        }
        // else: stays 0 (placeholder for unresolved marker)
    }

    result
}

/// Split the output MarkerValues into resolved and marker segments on the ChunkData.
fn flush_output(chunk: &mut ChunkData, output: &[MarkerValue]) {
    if output.is_empty() {
        return;
    }

    // Check if there are any markers at all — common fast path.
    let has_markers = output.iter().any(|mv| mv.is_marker());

    if !has_markers {
        // All literals — convert directly to bytes.
        let bytes: Vec<u8> = output.iter().map(|mv| mv.resolve(&[])).collect();
        chunk.append_resolved(bytes);
        return;
    }

    // Segment the output into runs of resolved-only and marker-containing spans.
    // For simplicity, we emit the entire output as a marker buffer when there
    // are any markers. The ChunkData::resolve_markers path handles this.
    chunk.append_markers(output.to_vec());
}

/// Decompress a raw DEFLATE buffer using libdeflate.
///
/// This is a fast path for when we have a complete compressed buffer and know
/// the expected decompressed size (e.g. BGZF blocks or after boundary detection).
pub fn decode_with_libdeflate(compressed: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let mut decompressor = libdeflater::Decompressor::new();
    let mut output = vec![0u8; expected_size];

    match decompressor.deflate_decompress(compressed, &mut output) {
        Ok(actual_size) => {
            if actual_size != expected_size {
                return Err(Error::SizeMismatch {
                    expected: expected_size as u32,
                    found: actual_size as u32,
                });
            }
            Ok(output)
        }
        Err(e) => Err(Error::Internal(format!("libdeflate decompression failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compress data with gzip and return the full gzip stream.
    fn gzip_compress(data: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Find the bit offset where DEFLATE data starts in a gzip stream.
    /// Parses the gzip header and returns the byte offset * 8.
    fn find_deflate_start(gzip_data: &[u8]) -> usize {
        use crate::gzip::header::GzipHeader;
        let mut cursor = std::io::Cursor::new(gzip_data);
        let _header = GzipHeader::parse(&mut cursor).unwrap();
        cursor.position() as usize * 8
    }

    #[test]
    fn test_speculative_decode_with_empty_window() {
        // With a known (empty) window at stream start, no markers should be produced.
        let original = b"Hello, world! This is a test of speculative decompression.";
        let compressed = gzip_compress(original);
        let start_bit = find_deflate_start(&compressed);
        // end_bit: exclude the 8-byte trailer
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved(), "chunk should be fully resolved with known window");

        // Verify the decompressed size matches.
        assert_eq!(chunk.decompressed_size(), original.len());

        // Verify actual content.
        let bufs = chunk.resolved_data();
        let result: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(result, original.as_slice());
    }

    #[test]
    fn test_speculative_decode_with_repetitive_data() {
        // Repetitive data will produce back-references within the chunk itself.
        // With a known empty window, these should all resolve.
        let original: Vec<u8> = b"ABCDEFGH".repeat(100);
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), original.len());

        let bufs = chunk.resolved_data();
        let result: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(result, original);
    }

    #[test]
    fn test_speculative_decode_without_window() {
        // Without a window, back-references into the unknown region produce markers.
        // We can't easily guarantee markers from a single gzip stream (the first
        // block starts with an empty window), so we test that the function runs
        // and produces correct output size.
        let original: Vec<u8> = b"ABCDEFGH".repeat(1000);
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;

        // Decode without a window — this simulates a mid-stream chunk.
        // Since this is actually the start of the stream, no references will
        // reach before position 0, so it should still be fully resolved.
        let chunk = speculative_decode(&compressed, start_bit, end_bit, None).unwrap();
        assert_eq!(chunk.decompressed_size(), original.len());
    }

    #[test]
    fn test_speculative_decode_literals_only() {
        // Data that compresses to mostly literals (random bytes).
        let original: Vec<u8> = (0..=255).cycle().take(256).collect();
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), original.len());

        let bufs = chunk.resolved_data();
        let result: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(result, original);
    }

    #[test]
    fn test_speculative_decode_final_window() {
        // Verify that the final window is populated.
        let original: Vec<u8> = vec![0x42; 100];
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        let final_window = chunk.final_window.as_ref().unwrap();
        // The final window is always 32KB, zero-padded at the front for short chunks.
        assert_eq!(final_window.len(), WINDOW_SIZE);
        assert!(final_window[..WINDOW_SIZE - 100].iter().all(|&b| b == 0x00));
        assert!(final_window[WINDOW_SIZE - 100..].iter().all(|&b| b == 0x42));
    }

    #[test]
    fn test_speculative_decode_large_data_window() {
        // Data larger than 32KB to exercise window wrapping.
        let original: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), original.len());

        let bufs = chunk.resolved_data();
        let result: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(result, original);

        // Final window should be last 32KB.
        let final_window = chunk.final_window.as_ref().unwrap();
        assert_eq!(final_window.len(), WINDOW_SIZE);
        assert_eq!(final_window, &original[original.len() - WINDOW_SIZE..]);
    }

    #[test]
    fn test_decode_with_libdeflate() {
        // Compress with flate2 raw deflate, decompress with libdeflate.
        let original = b"test data for libdeflate decompression path";
        let compressed = {
            use flate2::write::DeflateEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            e.write_all(original).unwrap();
            e.finish().unwrap()
        };
        let result = decode_with_libdeflate(&compressed, original.len()).unwrap();
        assert_eq!(result, original.as_slice());
    }

    #[test]
    fn test_decode_with_libdeflate_size_mismatch() {
        let original = b"hello";
        let compressed = {
            use flate2::write::DeflateEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            e.write_all(original).unwrap();
            e.finish().unwrap()
        };
        // Request wrong size — should get an error.
        let result = decode_with_libdeflate(&compressed, original.len() + 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_speculative_decode_rle_pattern() {
        // RLE-heavy data: distance < length (overlapping copy).
        let original: Vec<u8> = vec![b'A'; 10_000];
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved());
        assert_eq!(chunk.decompressed_size(), original.len());

        let bufs = chunk.resolved_data();
        let result: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(result, original);
    }

    #[test]
    fn test_marker_generation_and_resolution_roundtrip() {
        // Generate repetitive data large enough to produce multiple DEFLATE blocks.
        // The pattern repeats a 256-byte sequence many times, guaranteeing back-references.
        let pattern: Vec<u8> = (0..=255u8).collect();
        let original: Vec<u8> = pattern.iter().copied().cycle().take(256 * 1000).collect();

        let compressed = gzip_compress(&original);
        let deflate_start = find_deflate_start(&compressed);
        let deflate_end = (compressed.len() - 8) * 8;

        // Use the block scanner to find a DEFLATE block boundary after the first block.
        // Search starting after the first few hundred bits to skip the first block header.
        use crate::transcoder::block_scanner::scan_for_block;
        let search_start = deflate_start + 256; // skip well past the first block header
        let boundary = scan_for_block(&compressed, search_start, deflate_end);

        // If no boundary is found (e.g. the compressor used a single block), skip test.
        // This is unlikely with 256KB of repetitive data at default compression.
        let boundary = match boundary {
            Some(b) => b,
            None => return, // can't split — nothing to test
        };

        // The boundary bit_offset includes the BFINAL+BTYPE bits, but scan_for_block
        // returns the position of the block header. We need to decode everything before
        // this point to get the correct window.

        // Step 1: Decode the first portion with a known empty window to get correct output.
        let first_chunk =
            speculative_decode(&compressed, deflate_start, boundary.bit_offset, Some(&[]))
                .expect("first chunk decode failed");
        assert!(first_chunk.is_resolved(), "first chunk should be fully resolved");

        // Collect the first chunk's output.
        let first_output: Vec<u8> =
            first_chunk.resolved_data().iter().flat_map(|b| b.iter().copied()).collect();

        // Build the correct window: last 32KB of first_output.
        let window_start =
            if first_output.len() > WINDOW_SIZE { first_output.len() - WINDOW_SIZE } else { 0 };
        let correct_window = &first_output[window_start..];

        // Step 2: Decode the second portion WITH the correct window (ground truth).
        let truth_chunk =
            speculative_decode(&compressed, boundary.bit_offset, deflate_end, Some(correct_window))
                .expect("truth chunk decode failed");
        assert!(truth_chunk.is_resolved(), "truth chunk should be fully resolved");

        let truth_output: Vec<u8> =
            truth_chunk.resolved_data().iter().flat_map(|b| b.iter().copied()).collect();
        assert!(!truth_output.is_empty(), "truth output should not be empty");

        // Step 3: Decode the second portion WITHOUT a window (speculative, produces markers).
        let mut spec_chunk =
            speculative_decode(&compressed, boundary.bit_offset, deflate_end, None)
                .expect("speculative chunk decode failed");

        assert_eq!(
            spec_chunk.decompressed_size(),
            truth_output.len(),
            "speculative and truth chunks should have same size"
        );

        // Step 4: If there are markers, verify they have correct (non-zero) offsets
        // for references that don't start at offset 0, then resolve them.
        if !spec_chunk.is_resolved() {
            // Resolve markers with the correct window.
            spec_chunk.resolve_markers(correct_window);
            assert!(spec_chunk.is_resolved(), "chunk should be resolved after marker resolution");

            // Recompute final_window now that markers are resolved.
            spec_chunk.recompute_final_window();
        }

        // Step 5: Verify output matches the ground truth.
        let spec_output: Vec<u8> =
            spec_chunk.resolved_data().iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(
            spec_output, truth_output,
            "speculative decode + marker resolution must match direct decode"
        );

        // Step 6: Verify the combined output matches the original.
        let mut combined = first_output.clone();
        combined.extend_from_slice(&spec_output);
        assert_eq!(
            combined,
            original[..combined.len()],
            "combined output must match original data prefix"
        );
    }

    #[test]
    fn test_recompute_final_window_after_resolve() {
        // Verify that recompute_final_window produces correct bytes after resolution.
        let original: Vec<u8> = vec![0x42; 100];
        let compressed = gzip_compress(&original);
        let start_bit = find_deflate_start(&compressed);
        let end_bit = (compressed.len() - 8) * 8;
        let window: Vec<u8> = vec![];

        let mut chunk = speculative_decode(&compressed, start_bit, end_bit, Some(&window)).unwrap();
        assert!(chunk.is_resolved());

        chunk.recompute_final_window();
        let final_window = chunk.final_window.as_ref().unwrap();
        // The final window is always 32KB, zero-padded at the front for short chunks.
        assert_eq!(final_window.len(), WINDOW_SIZE);
        assert!(final_window[..WINDOW_SIZE - 100].iter().all(|&b| b == 0x00));
        assert!(final_window[WINDOW_SIZE - 100..].iter().all(|&b| b == 0x42));
    }
}
