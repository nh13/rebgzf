//! End-to-end integration tests for rebgzf.
//!
//! Tests all code paths with synthetic data to ensure correctness.

use std::io::{Cursor, Read, Write};
use std::process::Command;

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use rebgzf::{
    is_bgzf, validate_bgzf_streaming, validate_bgzf_strict, verify_bgzf, ParallelTranscoder,
    SingleThreadedTranscoder, TranscodeConfig, Transcoder,
};

// ============================================================================
// Test Data Generators
// ============================================================================

/// Generate random data using a simple PRNG
fn generate_random_data(size: usize, seed: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut state = seed;
    for _ in 0..size {
        // Simple xorshift PRNG
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.push((state & 0xFF) as u8);
    }
    data
}

/// Generate highly repetitive data (good compression)
fn generate_repetitive_data(size: usize) -> Vec<u8> {
    let pattern = b"AAAAAAAAAAAAAAAA";
    pattern.iter().cycle().take(size).copied().collect()
}

/// Generate data with mixed patterns (moderate compression)
fn generate_mixed_data(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let patterns = [
        b"ACGTACGTACGTACGT".as_slice(),
        b"NNNNNNNNNNNNNNNN".as_slice(),
        b"ATATATATATATATAT".as_slice(),
    ];

    let mut pattern_idx = 0;
    while data.len() < size {
        let pattern = patterns[pattern_idx % patterns.len()];
        let remaining = size - data.len();
        let chunk_size = remaining.min(pattern.len());
        data.extend_from_slice(&pattern[..chunk_size]);
        pattern_idx += 1;
    }
    data
}

/// Generate FASTQ-formatted data
fn generate_fastq_data(num_reads: usize, read_length: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let bases = [b'A', b'C', b'G', b'T'];

    for i in 0..num_reads {
        // Header
        writeln!(data, "@read_{}", i).unwrap();

        // Sequence - use deterministic pattern based on read number
        for j in 0..read_length {
            data.push(bases[(i + j) % 4]);
        }
        data.push(b'\n');

        // Plus line
        data.extend_from_slice(b"+\n");

        // Quality scores
        data.resize(data.len() + read_length, b'I'); // High quality
        data.push(b'\n');
    }
    data
}

/// Compress data to gzip format
fn compress_to_gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Decompress gzip/BGZF data to verify contents
/// Uses MultiGzDecoder to handle BGZF's concatenated gzip blocks
fn decompress_gzip(data: &[u8]) -> Vec<u8> {
    let mut decoder = MultiGzDecoder::new(data);
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
}

// ============================================================================
// BGZF Validation Helpers
// ============================================================================

/// Verify that output is valid BGZF format
fn verify_bgzf_format(data: &[u8]) -> bool {
    if data.len() < 18 {
        return false;
    }

    // Check gzip magic
    if data[0] != 0x1f || data[1] != 0x8b {
        return false;
    }

    // Check FEXTRA flag
    if data[3] & 0x04 == 0 {
        return false;
    }

    // Check BC subfield
    if data[12] != b'B' || data[13] != b'C' {
        return false;
    }

    true
}

/// Parse BGZF blocks and return block count and sizes
fn parse_bgzf_blocks(data: &[u8]) -> Vec<(usize, u32)> {
    let mut blocks = Vec::new();
    let mut pos = 0;

    while pos + 18 <= data.len() {
        // Check header
        if data[pos] != 0x1f || data[pos + 1] != 0x8b {
            break;
        }

        // Get BSIZE
        let bsize = u16::from_le_bytes([data[pos + 16], data[pos + 17]]) as usize + 1;

        if pos + bsize > data.len() {
            break;
        }

        // Get ISIZE (uncompressed size) from footer
        let isize = u32::from_le_bytes([
            data[pos + bsize - 4],
            data[pos + bsize - 3],
            data[pos + bsize - 2],
            data[pos + bsize - 1],
        ]);

        blocks.push((bsize, isize));
        pos += bsize;
    }

    blocks
}

// ============================================================================
// Single-Threaded Transcoder Tests
// ============================================================================

#[test]
fn test_single_thread_empty_input() {
    let data = Vec::new();
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig {
        block_size: 32768, // Smaller block size for safety
        ..Default::default()
    };
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let _stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    // Output should contain at least the EOF block
    assert!(!output.is_empty());
}

#[test]
fn test_single_thread_small_input() {
    let data = b"Hello, World!".to_vec();
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    assert!(stats.blocks_written >= 1);
}

#[test]
fn test_single_thread_exactly_one_block() {
    // Create data that fits in exactly one BGZF block (< 65280 bytes)
    let data = generate_random_data(60000, 12345);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    assert_eq!(stats.blocks_written, 1);
}

#[test]
fn test_single_thread_multiple_blocks() {
    // Create data that spans multiple BGZF blocks (use compressible data)
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    assert!(stats.blocks_written >= 3); // Should span at least 3 blocks
}

#[test]
fn test_single_thread_highly_compressible() {
    let data = generate_repetitive_data(500_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    // Highly compressible data should produce smaller output
    assert!(stats.output_bytes < data.len() as u64);
}

#[test]
fn test_single_thread_incompressible() {
    // Random data is incompressible and may expand during encoding
    // Use smaller block size to avoid exceeding BGZF max block size
    let data = generate_random_data(50_000, 99999);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig {
        block_size: 32768, // Smaller to accommodate potential expansion
        ..Default::default()
    };
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_single_thread_fastq_data() {
    let data = generate_fastq_data(1000, 150);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_single_thread_custom_block_size() {
    let data = generate_random_data(100_000, 11111);
    let gzip_data = compress_to_gzip(&data);

    // Use smaller block size
    let config = TranscodeConfig { block_size: 16384, ..Default::default() };
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    // Smaller block size should produce more blocks
    assert!(stats.blocks_written >= 6);
}

// ============================================================================
// Parallel Transcoder Tests
// ============================================================================

#[test]
fn test_parallel_2_threads() {
    let data = generate_mixed_data(500_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { num_threads: 2, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_parallel_4_threads() {
    let data = generate_fastq_data(5000, 150);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { num_threads: 4, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_parallel_8_threads() {
    let data = generate_random_data(1_000_000, 77777);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { num_threads: 8, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_parallel_matches_single_threaded() {
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    // Single-threaded
    let config_single = TranscodeConfig { num_threads: 1, ..Default::default() };
    let mut transcoder_single = SingleThreadedTranscoder::new(config_single);
    let mut output_single = Vec::new();
    transcoder_single.transcode(Cursor::new(&gzip_data), &mut output_single).unwrap();

    // Parallel
    let config_parallel = TranscodeConfig { num_threads: 4, ..Default::default() };
    let mut transcoder_parallel = ParallelTranscoder::new(config_parallel);
    let mut output_parallel = Vec::new();
    transcoder_parallel.transcode(Cursor::new(&gzip_data), &mut output_parallel).unwrap();

    // Both should decompress to the same data
    assert_eq!(decompress_gzip(&output_single), data);
    assert_eq!(decompress_gzip(&output_parallel), data);
}

#[test]
fn test_parallel_auto_threads() {
    let data = generate_random_data(100_000, 33333);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig {
        num_threads: 0, // Auto-detect
        ..Default::default()
    };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

// ============================================================================
// BGZF Detection Tests
// ============================================================================

#[test]
fn test_is_bgzf_detects_bgzf() {
    let data = generate_random_data(50_000, 12345);
    let gzip_data = compress_to_gzip(&data);

    // Transcode to BGZF
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Detect BGZF
    let mut cursor = Cursor::new(&bgzf_data);
    assert!(is_bgzf(&mut cursor).unwrap());
}

#[test]
fn test_is_bgzf_rejects_plain_gzip() {
    let data = b"Hello, World!";
    let gzip_data = compress_to_gzip(data);

    let mut cursor = Cursor::new(&gzip_data);
    assert!(!is_bgzf(&mut cursor).unwrap());
}

#[test]
fn test_is_bgzf_rejects_random_data() {
    let random_data = generate_random_data(1000, 99999);

    let mut cursor = Cursor::new(&random_data);
    assert!(!is_bgzf(&mut cursor).unwrap());
}

#[test]
fn test_is_bgzf_empty_input() {
    let empty: Vec<u8> = Vec::new();

    let mut cursor = Cursor::new(&empty);
    assert!(!is_bgzf(&mut cursor).unwrap());
}

#[test]
fn test_validate_bgzf_strict_valid() {
    // Use compressible data to avoid block size issues
    let data = generate_mixed_data(100_000);
    let gzip_data = compress_to_gzip(&data);

    // Transcode to BGZF
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Strict validation
    let mut cursor = Cursor::new(&bgzf_data);
    let validation = validate_bgzf_strict(&mut cursor).unwrap();

    assert!(validation.is_valid_bgzf);
    assert!(validation.block_count.is_some());
    assert!(validation.block_count.unwrap() >= 2); // Data + EOF
    assert!(validation.total_uncompressed_size.is_some());
}

#[test]
fn test_validate_bgzf_strict_invalid() {
    let gzip_data = compress_to_gzip(b"Hello");

    let mut cursor = Cursor::new(&gzip_data);
    let validation = validate_bgzf_strict(&mut cursor).unwrap();

    assert!(!validation.is_valid_bgzf);
}

// ============================================================================
// Round-Trip Property Tests
// ============================================================================

/// Property test: any data compressed to gzip then transcoded to BGZF
/// should decompress to the original data.
#[test]
fn test_roundtrip_property_random_data() {
    // Test with various sizes
    // Use smaller block size for random/incompressible data to avoid exceeding BGZF max
    for size in [0, 1, 10, 100, 1000, 10000, 100000] {
        let data = generate_random_data(size, size as u64);
        let gzip_data = compress_to_gzip(&data);

        // Transcode to BGZF with smaller block size for incompressible data
        let config = TranscodeConfig { block_size: 32768, ..Default::default() };
        let mut transcoder = SingleThreadedTranscoder::new(config);
        let mut bgzf_data = Vec::new();
        transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

        // Decompress and verify
        let decompressed = decompress_gzip(&bgzf_data);
        assert_eq!(decompressed, data, "Round-trip failed for size {}", size);
    }
}

#[test]
fn test_roundtrip_property_repetitive_data() {
    for size in [100, 1000, 10000, 50000] {
        let data = generate_repetitive_data(size);
        let gzip_data = compress_to_gzip(&data);

        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);
        let mut bgzf_data = Vec::new();
        transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

        let decompressed = decompress_gzip(&bgzf_data);
        assert_eq!(decompressed, data, "Round-trip failed for repetitive data size {}", size);
    }
}

#[test]
fn test_roundtrip_property_fastq_data() {
    for num_reads in [10, 100, 1000] {
        let data = generate_fastq_data(num_reads, 150);
        let gzip_data = compress_to_gzip(&data);

        let config = TranscodeConfig::default();
        let mut transcoder = SingleThreadedTranscoder::new(config);
        let mut bgzf_data = Vec::new();
        transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

        let decompressed = decompress_gzip(&bgzf_data);
        assert_eq!(decompressed, data, "Round-trip failed for FASTQ {} reads", num_reads);
    }
}

#[test]
fn test_roundtrip_property_all_bytes() {
    // Test that all byte values survive the round-trip
    let mut data = Vec::with_capacity(256 * 10);
    for _ in 0..10 {
        for b in 0..=255u8 {
            data.push(b);
        }
    }

    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    let decompressed = decompress_gzip(&bgzf_data);
    assert_eq!(decompressed, data, "All-bytes round-trip failed");
}

#[test]
fn test_roundtrip_property_parallel() {
    // Test parallel transcoding preserves data
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    for threads in [2, 4, 8] {
        let config = TranscodeConfig { num_threads: threads, ..Default::default() };
        let mut transcoder = ParallelTranscoder::new(config);
        let mut bgzf_data = Vec::new();
        transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

        let decompressed = decompress_gzip(&bgzf_data);
        assert_eq!(decompressed, data, "Parallel round-trip failed with {} threads", threads);
    }
}

#[test]
fn test_roundtrip_property_compression_levels() {
    // Test all compression levels preserve data
    use rebgzf::CompressionLevel;

    let data = generate_mixed_data(50_000);
    let gzip_data = compress_to_gzip(&data);

    for level in 1..=9 {
        let config = TranscodeConfig {
            compression_level: CompressionLevel::from_level(level),
            ..Default::default()
        };
        let mut transcoder = SingleThreadedTranscoder::new(config);
        let mut bgzf_data = Vec::new();
        transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

        let decompressed = decompress_gzip(&bgzf_data);
        assert_eq!(decompressed, data, "Round-trip failed at compression level {}", level);
    }
}

// ============================================================================
// BGZF Verification Tests (Deep validation with CRC check)
// ============================================================================

#[test]
fn test_verify_bgzf_valid() {
    // Use compressible data
    let data = generate_mixed_data(100_000);
    let gzip_data = compress_to_gzip(&data);

    // Transcode to BGZF
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Deep verification
    let mut cursor = Cursor::new(&bgzf_data);
    let verification = verify_bgzf(&mut cursor).unwrap();

    assert!(verification.is_valid_bgzf, "Should be valid BGZF");
    assert!(verification.crc_valid, "CRC32 checksums should match");
    assert!(verification.isize_valid, "ISIZE values should match");
    assert!(verification.block_count >= 2, "Should have data + EOF blocks");
    assert_eq!(
        verification.uncompressed_size as usize,
        data.len(),
        "Uncompressed size should match original data"
    );
    assert!(verification.first_error.is_none(), "Should have no errors");
}

#[test]
fn test_verify_bgzf_invalid_not_bgzf() {
    // Plain gzip is not valid BGZF
    let gzip_data = compress_to_gzip(b"Hello");

    let mut cursor = Cursor::new(&gzip_data);
    let verification = verify_bgzf(&mut cursor).unwrap();

    assert!(!verification.is_valid_bgzf, "Plain gzip should not be valid BGZF");
    assert!(verification.first_error.is_some(), "Should have error message");
}

// ============================================================================
// Block Structure Tests
// ============================================================================

#[test]
fn test_bgzf_blocks_have_correct_structure() {
    // Use compressible data to avoid block size issues
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    let blocks = parse_bgzf_blocks(&output);

    // Verify we have multiple blocks
    assert!(blocks.len() >= 3);

    // Verify all blocks (except EOF) have reasonable uncompressed sizes
    for (i, (bsize, isize)) in blocks.iter().enumerate() {
        assert!(*bsize <= 65536, "Block {} too large: {}", i, bsize);
        if i < blocks.len() - 1 {
            // Non-EOF blocks should have data
            assert!(*isize > 0 || *bsize == 28, "Block {} has no data", i);
        }
    }

    // Last block should be EOF (28 bytes, 0 uncompressed)
    let (last_bsize, last_isize) = blocks.last().unwrap();
    assert_eq!(*last_bsize, 28);
    assert_eq!(*last_isize, 0);
}

#[test]
fn test_bgzf_total_uncompressed_matches() {
    let data = generate_mixed_data(150_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    let blocks = parse_bgzf_blocks(&output);

    // Sum of all ISIZE values should equal original data size
    let total_uncompressed: u64 = blocks.iter().map(|(_, isize)| *isize as u64).sum();
    assert_eq!(total_uncompressed, data.len() as u64);
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_data_at_block_boundary() {
    // Create compressible data at block size boundary
    let data = generate_mixed_data(65280);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_data_just_over_block_boundary() {
    // Create compressible data just over block size boundary
    let data = generate_mixed_data(65281);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    assert!(stats.blocks_written >= 2); // Should split into 2+ blocks
}

#[test]
fn test_large_data() {
    // Test with 500KB of data
    let data = generate_random_data(500_000, 88888);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { num_threads: 4, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
    assert!(stats.blocks_written >= 7); // ~500KB / 65KB blocks
}

#[test]
fn test_single_byte_input() {
    let data = vec![0x42];
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

// ============================================================================
// Stats Verification
// ============================================================================

#[test]
fn test_stats_accuracy() {
    // Use compressible data
    let data = generate_mixed_data(100_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    // Input bytes should be approximately the compressed input size (may differ slightly)
    assert!(stats.input_bytes > 0);
    assert!(stats.input_bytes <= gzip_data.len() as u64 + 100); // Allow small tolerance

    // Output bytes should match actual output size
    assert_eq!(stats.output_bytes, output.len() as u64);

    // Block count should match parsed blocks (excluding EOF marker which has isize=0)
    let blocks = parse_bgzf_blocks(&output);
    let data_blocks = blocks.iter().filter(|(_, isize)| *isize > 0).count();
    assert_eq!(stats.blocks_written, data_blocks as u64);
}

// ============================================================================
// Compression Level Input Tests
// ============================================================================

#[test]
fn test_input_compression_level_1() {
    let data = generate_mixed_data(100_000);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&data).unwrap();
    let gzip_data = encoder.finish().unwrap();

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

#[test]
fn test_input_compression_level_9() {
    let data = generate_mixed_data(100_000);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&data).unwrap();
    let gzip_data = encoder.finish().unwrap();

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    assert!(verify_bgzf_format(&output));
    assert_eq!(decompress_gzip(&output), data);
}

// ============================================================================
// Concatenated Gzip Tests
// ============================================================================

#[test]
fn test_concatenated_gzip_single_threaded() {
    // Create three separate gzip streams and concatenate them
    let data1 = generate_mixed_data(50_000);
    let data2 = generate_random_data(30_000, 42);
    let data3 = generate_repetitive_data(40_000);

    let gzip1 = compress_to_gzip(&data1);
    let gzip2 = compress_to_gzip(&data2);
    let gzip3 = compress_to_gzip(&data3);

    // Concatenate the gzip streams
    let mut concat_gzip = Vec::new();
    concat_gzip.extend_from_slice(&gzip1);
    concat_gzip.extend_from_slice(&gzip2);
    concat_gzip.extend_from_slice(&gzip3);

    // Transcode
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();
    let stats = transcoder.transcode(Cursor::new(&concat_gzip), &mut output).unwrap();

    // Verify output is valid BGZF
    assert!(verify_bgzf_format(&output));

    // Verify decompressed content matches all three inputs concatenated
    let expected: Vec<u8> = [data1, data2, data3].concat();
    assert_eq!(decompress_gzip(&output), expected);

    // Verify we processed more than just the first member
    assert!(stats.input_bytes > gzip1.len() as u64);
}

#[test]
fn test_concatenated_gzip_parallel() {
    // Create two separate gzip streams
    let data1 = generate_fastq_data(500, 100);
    let data2 = generate_mixed_data(60_000);

    let gzip1 = compress_to_gzip(&data1);
    let gzip2 = compress_to_gzip(&data2);

    // Concatenate
    let mut concat_gzip = Vec::new();
    concat_gzip.extend_from_slice(&gzip1);
    concat_gzip.extend_from_slice(&gzip2);

    // Transcode with 2 threads
    let config = TranscodeConfig { num_threads: 2, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();
    let stats = transcoder.transcode(Cursor::new(&concat_gzip), &mut output).unwrap();

    // Verify output
    assert!(verify_bgzf_format(&output));

    let expected: Vec<u8> = [data1, data2].concat();
    assert_eq!(decompress_gzip(&output), expected);

    // Verify we processed both members
    assert!(stats.input_bytes > gzip1.len() as u64);
}

// ============================================================================
// Binary CLI Tests (if binary is built)
// ============================================================================

#[test]
#[ignore] // Run with --ignored flag when binary is available
fn test_cli_check_bgzf() {
    // This test requires the binary to be built
    let data = generate_random_data(10_000, 12345);
    let gzip_data = compress_to_gzip(&data);

    // Create BGZF first
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Write to temp file and test CLI
    let temp_dir = std::env::temp_dir();
    let bgzf_path = temp_dir.join("test_cli.bgzf");
    std::fs::write(&bgzf_path, &bgzf_data).unwrap();

    let output = Command::new("cargo")
        .args(["run", "--bin", "rebgzf", "--", "--check", "-i"])
        .arg(&bgzf_path)
        .output()
        .expect("Failed to run CLI");

    assert!(output.status.success(), "CLI should return 0 for BGZF input");

    std::fs::remove_file(&bgzf_path).ok();
}

#[test]
#[ignore] // Run with --ignored flag when binary is available
fn test_cli_check_gzip() {
    let data = b"Hello, World!";
    let gzip_data = compress_to_gzip(data);

    let temp_dir = std::env::temp_dir();
    let gzip_path = temp_dir.join("test_cli.gz");
    std::fs::write(&gzip_path, &gzip_data).unwrap();

    let output = Command::new("cargo")
        .args(["run", "--bin", "rebgzf", "--", "--check", "-i"])
        .arg(&gzip_path)
        .output()
        .expect("Failed to run CLI");

    // Should return non-zero for non-BGZF input
    assert!(!output.status.success(), "CLI should return 1 for gzip input");

    std::fs::remove_file(&gzip_path).ok();
}

// ============================================================================
// Index Generation Tests
// ============================================================================

#[test]
fn test_index_generation_single_thread() {
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { build_index: true, ..Default::default() };
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    // Should have index entries
    assert!(stats.index_entries.is_some());
    let entries = stats.index_entries.unwrap();

    // Should have entries for each data block
    assert_eq!(entries.len(), stats.blocks_written as usize);

    // First entry should be at offset 0
    assert_eq!(entries[0].compressed_offset, 0);
    assert_eq!(entries[0].uncompressed_offset, 0);

    // Entries should be in ascending order
    for i in 1..entries.len() {
        assert!(entries[i].compressed_offset > entries[i - 1].compressed_offset);
        assert!(entries[i].uncompressed_offset > entries[i - 1].uncompressed_offset);
    }
}

#[test]
fn test_index_generation_parallel() {
    let data = generate_mixed_data(200_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig { build_index: true, num_threads: 4, ..Default::default() };
    let mut transcoder = ParallelTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    // Should have index entries
    assert!(stats.index_entries.is_some());
    let entries = stats.index_entries.unwrap();

    // Should have entries for each data block
    assert_eq!(entries.len(), stats.blocks_written as usize);

    // Verify entries are consistent with block structure
    let blocks = parse_bgzf_blocks(&output);
    let mut expected_compressed = 0u64;
    let mut expected_uncompressed = 0u64;

    for (i, (bsize, isize)) in blocks.iter().take(entries.len()).enumerate() {
        assert_eq!(
            entries[i].compressed_offset, expected_compressed,
            "Entry {} compressed offset mismatch",
            i
        );
        assert_eq!(
            entries[i].uncompressed_offset, expected_uncompressed,
            "Entry {} uncompressed offset mismatch",
            i
        );
        expected_compressed += *bsize as u64;
        expected_uncompressed += *isize as u64;
    }
}

#[test]
fn test_index_disabled_by_default() {
    let data = generate_mixed_data(100_000);
    let gzip_data = compress_to_gzip(&data);

    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut output = Vec::new();

    let stats = transcoder.transcode(Cursor::new(&gzip_data), &mut output).unwrap();

    // Should not have index entries by default
    assert!(stats.index_entries.is_none());
}

// ============================================================================
// Streaming Validation Tests
// ============================================================================

#[test]
fn test_streaming_validation_valid_bgzf() {
    let data = generate_mixed_data(100_000);
    let gzip_data = compress_to_gzip(&data);

    // Create BGZF
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Validate via streaming (simulating stdin)
    let mut cursor = Cursor::new(&bgzf_data);
    let validation = validate_bgzf_streaming(&mut cursor).unwrap();

    assert!(validation.is_valid_bgzf);
    assert!(validation.block_count.is_some());
    assert!(validation.total_uncompressed_size.is_some());
    assert_eq!(validation.total_uncompressed_size.unwrap(), data.len() as u64);
}

#[test]
fn test_streaming_validation_matches_strict() {
    let data = generate_mixed_data(150_000);
    let gzip_data = compress_to_gzip(&data);

    // Create BGZF
    let config = TranscodeConfig::default();
    let mut transcoder = SingleThreadedTranscoder::new(config);
    let mut bgzf_data = Vec::new();
    transcoder.transcode(Cursor::new(&gzip_data), &mut bgzf_data).unwrap();

    // Validate both ways
    let mut cursor_streaming = Cursor::new(&bgzf_data);
    let streaming = validate_bgzf_streaming(&mut cursor_streaming).unwrap();

    let mut cursor_strict = Cursor::new(&bgzf_data);
    let strict = validate_bgzf_strict(&mut cursor_strict).unwrap();

    // Results should match
    assert_eq!(streaming.is_valid_bgzf, strict.is_valid_bgzf);
    assert_eq!(streaming.block_count, strict.block_count);
    assert_eq!(streaming.total_uncompressed_size, strict.total_uncompressed_size);
}

#[test]
fn test_streaming_validation_invalid() {
    let gzip_data = compress_to_gzip(b"Hello, World!");

    let mut cursor = Cursor::new(&gzip_data);
    let validation = validate_bgzf_streaming(&mut cursor).unwrap();

    assert!(!validation.is_valid_bgzf);
}
