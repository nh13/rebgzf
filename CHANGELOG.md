# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.0]: https://github.com/nh13/rebgzf/releases/tag/v0.1.0
