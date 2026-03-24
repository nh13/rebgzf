# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-03-24

### Added

- Parallel DEFLATE decode with rapidgzip-style block boundary scanning
  - Phase 1+2: Parallel scan for valid DEFLATE block boundaries + Huffman decode
  - Phase 3: Sequential boundary resolution + parallel BGZF encoding
  - Automatic fallback to single-threaded for small files or stdin
- Memory-mapped I/O with `SliceBitReader` for zero-copy DEFLATE parsing
- Multi-member gzip detection (falls back to single-threaded for correctness)
- Smart boundary splitting in parallel decode path (FASTQ record-aligned blocks)
- Shared encoding infrastructure module (`transcoder::encoding`)

### Performance

- **2.6x** single-thread throughput improvement (14 → 36 MB/s on 5.7 GB FASTQ gzip)
- **2.2x** multi-thread throughput improvement (32 → 72 MB/s at 4 threads)
- 12-bit Huffman lookup tables (8KB, fits L1 cache) with unsafe hot-path optimizations
- Precomputed reversed Huffman codes (eliminates per-token bit reversal)
- Linear decode buffer replacing circular SlidingWindow (SIMD-friendly single-call CRC)
- Fast-path non-RLE copies using `extend_from_slice` / `extend_from_within`
- Batch window updates in BoundaryResolver (`push_bytes` instead of per-byte loops)
- O(1) `encode_length` and binary-search `encode_distance` lookup tables
- CRC32 computed in resolver (eliminates `tokens_to_bytes` allocation in workers)
- Fused resolve+encode for fixed Huffman (single-threaded, eliminates intermediate Vec)
- Streaming token handoff from Phase 2 to Phase 3 (caps peak memory at ~1 chunk)

### Fixed

- Deadlock in parallel encode flush path at high thread counts (blocking `send` replaced
  with `send_job_and_drain`)
- Multi-member gzip files no longer produce corrupt output in parallel decode path
- NLEN validation in stored block handling for multi-member detection

### Changed

- `BoundaryResolver` uses a linear decode buffer instead of circular `SlidingWindow`
- `resolve_block` now returns CRC32 (previously only the single-threaded variant did)
- `LZ77Token` derives `Copy` (6-byte value type, no heap allocation)
- Minimum Rust version unchanged (1.70)

### Dependencies

- `thiserror` 1.0 → 2.0
- `clap` 4.5.53 → 4.5.60
- `flate2` 1.1.5 → 1.1.9
- `criterion` 0.5 → 0.8

## [0.1.0] - 2024-12-19

### Added

- Initial release of rebgzf - efficient gzip to BGZF transcoder
- Half-decompression technique inspired by Puffin for fast transcoding
- Compression levels 1-9:
  - Levels 1-3: Fixed Huffman tables (fastest)
  - Levels 4-6: Dynamic Huffman tables (better compression)
  - Levels 7-9: Dynamic Huffman with smart boundary splitting
- FASTQ-aware block splitting for record-aligned boundaries
- GZI index generation for random access support
- Parallel transcoding with configurable thread count
- Concatenated gzip file support
- BGZF detection and validation:
  - `--check`: Quick header check
  - `--strict`: Full block validation
  - `--verify`: Deep validation with CRC32 checking
- File statistics with `--stats`
- Progress display with `--progress`
- JSON output with `--json` for scripting
- Quiet mode with `-q/--quiet`
- Table-based Huffman decoding for ~30% parsing speedup
- Hardware-accelerated CRC32 via `crc32fast`
- Comprehensive test suite (100+ tests)
- Criterion benchmarks
- Fuzz testing infrastructure

### Performance

- Single-threaded: ~20 MB/s on compressed input
- Parallel (2 threads): ~32 MB/s on compressed input
- Memory-efficient: bounded by block size + thread count

[0.2.0]: https://github.com/nh13/rebgzf/releases/tag/v0.2.0
[0.1.0]: https://github.com/nh13/rebgzf/releases/tag/v0.1.0
