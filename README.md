# rebgzf

Efficient gzip to BGZF transcoder using half-decompression with parallel DEFLATE decode.

## Why BGZF?

[BGZF](https://samtools.github.io/hts-specs/SAMv1.pdf) (Blocked GNU Zip Format) is a variant of gzip that splits compressed data into independent ~64KB blocks. This enables:

- **Random access** — seek to any block without decompressing from the start
- **Parallel decompression** — tools can decompress blocks independently across threads
- **Streaming compatibility** — BGZF is valid gzip, so any gzip reader can open it

BGZF is the standard format for BAM, CRAM, and VCF in bioinformatics. Tools built on htslib, samtools, and similar libraries can decompress BGZF blocks in parallel, significantly speeding up read-heavy workloads.

### The Problem

Sequencing instruments typically produce plain gzip FASTQs, not BGZF. Ideally, vendors and basecallers would output BGZF from the start — but many don't. This means every downstream tool that reads the FASTQ is stuck decompressing a single gzip stream sequentially, even if the tool itself supports parallel BGZF decompression.

Converting gzip to BGZF traditionally requires full decompression followed by re-compression. This is slow because compression is the expensive part.

### The Solution

rebgzf transcodes gzip → BGZF fast enough to be used inline in pipelines:

```bash
# Before: tool decompresses gzip single-threaded (slow)
tool < reads.fastq.gz

# After: rebgzf transcodes in parallel, tool decompresses BGZF in parallel
rebgzf -i reads.fastq.gz -t 4 | tool
```

The total wall time drops because both the transcoding and the downstream decompression are parallelized. For tools that support BGZF-aware parallel I/O, this can be a substantial speedup even accounting for the transcoding overhead.

### The Half-Decompression Approach

This tool uses a **half-decompression** technique inspired by [Puffin](https://chromium.googlesource.com/chromiumos/platform/puffin/):

1. Parse the DEFLATE stream to extract **LZ77 tokens** (literals, lengths, distances)
2. Track only enough context to resolve **cross-boundary references**
3. **Re-encode** the tokens directly into new BGZF blocks

This avoids the expensive compression step entirely — we're just re-serializing already-compressed data into a different block structure.

### Performance

**Benchmark** (5.7 GB FASTQ gzip, Apple M1, level 1, low system load):

| Threads | Time   | Throughput | Speedup |
|---------|--------|------------|---------|
| 1       | 170s   | 36 MB/s    | 1.0x    |
| 4       | 86s    | 72 MB/s    | 2.0x   |

*Throughput measured on compressed input size. Multi-thread scaling depends on system load — the sequential boundary resolver is the bottleneck at high thread counts.*

Compared to v0.1.0 (14 MB/s single-threaded, 32 MB/s parallel), v0.2.0 achieves a **2.6x single-thread** and **2.2x multi-thread** improvement through parallel DEFLATE decode and resolver optimizations.

**Output size:** BGZF output is typically ~1.5x larger than the original gzip because each BGZF block has overhead, cross-boundary LZ77 references are expanded to literals, and fixed Huffman encoding is less optimal than the original dynamic tables.

## Installation

### From source

```bash
git clone https://github.com/nh13/rebgzf.git
cd rebgzf
cargo install --path .
```

### From crates.io (coming soon)

```bash
cargo install rebgzf
```

## Usage

### Command Line

```bash
# Basic transcoding (fastest, level 1)
rebgzf -i input.gz -o output.bgz

# Parallel transcoding (auto-detect threads)
rebgzf -i input.gz -o output.bgz -t 0 --progress

# Better compression with dynamic Huffman (level 6)
rebgzf -i input.gz -o output.bgz --level 6

# Best compression for FASTQ files (dynamic Huffman + record-aligned blocks)
rebgzf -i reads.fastq.gz -o reads.bgz --format fastq --level 9

# Generate GZI index for random access
rebgzf -i data.gz -o data.bgz --index

# Check if a file is already BGZF
rebgzf --check -i input.gz
echo $?  # 0 = BGZF, 1 = not BGZF

# Strict validation (all blocks, works with stdin)
cat file.bgz | rebgzf --check --strict -i -

# Force transcoding even if already BGZF
rebgzf -i input.bgz -o output.bgz --force

# Verbose output with statistics
rebgzf -i input.gz -o output.bgz -v
```

### CLI Options

<!-- start usage -->
```
Convert gzip files to BGZF format efficiently

Usage: rebgzf [OPTIONS] --input <INPUT>

Options:
  -i, --input <INPUT>            Input gzip file (use - for stdin)
  -o, --output <OUTPUT>          Output BGZF file (use - for stdout)
  -t, --threads <THREADS>        Number of threads (0 = auto, 1 = single-threaded) [default: 1]
  -l, --level <LEVEL>            Compression level 1-9 (1-3: fixed Huffman, 4-6: dynamic,
                                 7-9: dynamic + smart boundaries) [default: 1]
      --format <FORMAT>          Input format profile: default, fastq, auto [default: default]
      --block-size <BLOCK_SIZE>  BGZF block size (default: 65280) [default: 65280]
  -v, --verbose                  Show verbose statistics
  -q, --quiet                    Quiet mode - suppress all output except errors
      --json                     Output results as JSON (for scripting)
      --check                    Check if input is BGZF and exit (0=BGZF, 1=not BGZF, 2=error)
      --strict                   Validate all BGZF blocks (slower, more thorough)
      --verify                   Verify BGZF by decompressing and checking CRC32
      --stats                    Show file statistics without transcoding
      --force                    Force transcoding even if input is already BGZF
  -p, --progress                 Show progress during transcoding
      --index [PATH]             Write GZI index file (enables random access)
  -h, --help                     Print help
  -V, --version                  Print version
```
<!-- end usage -->

### As a Library

```rust
use rebgzf::{
    CompressionLevel, MappedFile, ParallelDecodeTranscoder,
    TranscodeConfig,
};
use std::fs::File;

fn main() -> rebgzf::Result<()> {
    let mmap = MappedFile::open("input.gz")?;
    let output = File::create("output.bgz")?;

    let config = TranscodeConfig {
        num_threads: 0,  // auto-detect
        compression_level: CompressionLevel::Level1,  // fastest
        ..Default::default()
    };

    let mut transcoder = ParallelDecodeTranscoder::new(config);
    let stats = transcoder.transcode_mmap(&mmap, output)?;

    println!("Transcoded {} bytes -> {} bytes ({} blocks)",
        stats.input_bytes,
        stats.output_bytes,
        stats.blocks_written
    );

    Ok(())
}
```

For streaming input (stdin, pipes), use `ParallelTranscoder` or `SingleThreadedTranscoder` with the `Transcoder` trait:

```rust
use rebgzf::{ParallelTranscoder, TranscodeConfig, Transcoder};

let config = TranscodeConfig { num_threads: 2, ..Default::default() };
let mut transcoder = ParallelTranscoder::new(config);
let stats = transcoder.transcode(input, output)?;
```

### BGZF Detection

```rust
use rebgzf::{is_bgzf, validate_bgzf_strict};
use std::fs::File;
use std::io::{Seek, SeekFrom};

fn main() -> rebgzf::Result<()> {
    let mut file = File::open("input.gz")?;

    // Quick check (reads first 18 bytes)
    if is_bgzf(&mut file)? {
        println!("File appears to be BGZF");
    }

    // Strict validation (validates all blocks, requires Seek)
    file.seek(SeekFrom::Start(0))?;
    let validation = validate_bgzf_strict(&mut file)?;
    if validation.is_valid_bgzf {
        println!("Valid BGZF with {} blocks, {} uncompressed bytes",
            validation.block_count.unwrap_or(0),
            validation.total_uncompressed_size.unwrap_or(0));
    }

    Ok(())
}
```

## How It Works

### DEFLATE Token Extraction

A DEFLATE stream consists of:
- **Literals**: raw bytes (0-255)
- **Back-references**: (length, distance) pairs pointing to earlier data
- **End-of-block**: marker indicating block completion

We parse the Huffman-coded stream to extract these LZ77 tokens without fully reconstructing the original data. The key insight is that we only need the tokens themselves, not the decompressed bytes.

### Cross-Boundary Reference Resolution

When splitting into BGZF blocks (~65KB uncompressed each), back-references may point across block boundaries. The `BoundaryResolver` uses a linear decode buffer to:
1. Detect when a reference's distance exceeds the current block's accumulated size
2. Resolve cross-boundary references by emitting the referenced bytes as literals
3. Preserve intra-block references as-is (they compress better)

CRC32 is computed in the resolver using a single `crc32fast::hash` call over each block's contiguous decoded bytes, enabling SIMD acceleration.

### Re-encoding

Tokens are re-encoded using either:

- **Fixed Huffman tables** (levels 1-3): Fast encoding using pre-defined tables. At level 1, the resolver and encoder are fused into a single pass (no intermediate token allocation).
- **Dynamic Huffman tables** (levels 4-9): Per-block optimal tables computed from token frequencies.

At levels 7-9 with `--format fastq`, block boundaries are aligned to FASTQ record boundaries for better compression.

## Architecture

### Single-Threaded Pipeline

```text
Input (gzip)
  │
  ├─ GzipHeader Parser
  │
  ├─ DEFLATE Parser (SliceBitReader for mmap, BitReader for streams)
  │    └─ 12-bit Huffman lookup table (8KB, L1 cache)
  │
  ├─ BoundaryResolver (linear decode buffer)
  │    ├─ Cross-boundary refs → expand to literals
  │    ├─ Within-block refs → preserve as Copy tokens
  │    ├─ CRC32 via single crc32fast::hash per block
  │    └─ Fused with encoder at level 1 (no intermediate Vec)
  │
  ├─ HuffmanEncoder (pre-reversed codes, O(1) length/distance lookup)
  │
  └─ BGZF Writer
       └─ Output (BGZF)
```

### Parallel Decode Pipeline (mmap input)

```text
Input (mmap'd gzip)
  │
  ├─ Parse gzip header, compute DEFLATE region
  │
  ├─ Phase 1+2 (parallel, N threads):
  │    ├─ Thread 0: decode from known DEFLATE start
  │    └─ Threads 1..N: scan for DEFLATE block boundaries
  │         using 6-stage validation (rapidgzip-style),
  │         probe-decode to reject false positives,
  │         then full decode from validated boundary
  │
  │    Tokens streamed to Phase 3 via Mutex+Condvar slots
  │    (peak memory: ~1 chunk, not entire file)
  │
  ├─ Phase 3 (main thread + worker pool):
  │    ├─ Main thread: consume chunks in order →
  │    │   BoundaryResolver → dispatch to workers
  │    └─ Workers: HuffmanEncoder → BGZF block assembly
  │         (CRC pre-computed by resolver, no tokens_to_bytes)
  │
  └─ Ordered output assembly → Output (BGZF)
```

For streaming input (stdin, pipes), the parallel encode pipeline is used instead: the main thread parses DEFLATE sequentially while workers encode BGZF blocks in parallel.

Multi-member (concatenated) gzip files are detected by decoding the first member's DEFLATE stream to find its end, then checking for another gzip header. Multi-member files fall back to single-threaded transcoding for correctness.

## Optimization Techniques

### 12-bit Huffman Lookup Table

Standard Huffman decoding reads one bit at a time. We use a 4096-entry lookup table (8KB, fits in L1 cache):
1. Peek 12 bits from the bitstream
2. Single table lookup returns both the symbol and its code length
3. Consume only the actual code length bits

Over 99% of DEFLATE codes resolve in one lookup. Codes longer than 12 bits fall back to bit-by-bit decoding.

### Pre-reversed Huffman Codes

DEFLATE stores Huffman codes MSB-first, but the bitstream is read LSB-first. Instead of reversing bits per-token in the encoding hot loop, codes are pre-reversed at tree construction time. The encoder calls `write_bits` directly — no per-token bit reversal.

### Linear Decode Buffer

The `BoundaryResolver` uses a linear buffer `[prev_tail: 32KB | current_block: ~65KB]` instead of a circular sliding window:
- Copy lookups are simple array indexing (no circular wrapping)
- Non-RLE copies use `extend_from_within` (bulk memcpy)
- CRC is computed in one `crc32fast::hash` call per block on the contiguous decoded bytes, enabling SIMD

### O(1) Encode Tables

Length and distance encoding uses precomputed lookup tables instead of linear scans. `encode_length` uses a 256-entry const table indexed by `length - 3`. `encode_distance` uses a 256-entry table for distances <= 256 and binary search (~5 comparisons) for larger distances.

### Streaming Token Handoff

Phase 2 decode threads deposit tokens into shared slots (one per chunk) with `Mutex`+`Condvar` notification. Phase 3 consumes them in order as they become available, dropping each chunk after processing. Peak memory is proportional to one chunk rather than the entire file — critical for 100GB+ inputs.

## Benchmarks

Run benchmarks with:

```bash
cargo bench
```

## Testing

```bash
# Run all tests
cargo test

# Run integration tests
cargo test --test integration

# Run with verbose output
cargo test -- --nocapture
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](docs/CONTRIBUTING.md) for guidelines.

## License

MIT License - see [LICENSE](LICENSE) for details.
