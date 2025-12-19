# Developer Guide

This guide covers setting up the development environment and working with the codebase.

## Prerequisites

- Rust 1.70 or later
- `cargo-nextest` (optional, for faster test runs)

## Setup

```bash
# Clone the repository
git clone https://github.com/nh13/rebgzf.git
cd rebgzf

# Build
cargo build

# Run tests
cargo test

# Build release binary
cargo build --release
```

## Project Structure

```
src/
├── lib.rs              # Library exports and main types
├── error.rs            # Error types
├── bin/
│   └── rebgzf.rs       # CLI binary
├── bits/
│   ├── mod.rs          # Bit-level I/O
│   ├── reader.rs       # Bitstream reader
│   └── writer.rs       # Bitstream writer
├── gzip/
│   ├── mod.rs          # Gzip format handling
│   └── header.rs       # Gzip header parsing
├── huffman/
│   ├── mod.rs          # Huffman coding
│   ├── tables.rs       # Fixed/dynamic Huffman tables
│   ├── decoder.rs      # Huffman decoding
│   └── encoder.rs      # Huffman encoding
├── deflate/
│   ├── mod.rs          # DEFLATE format
│   ├── parser.rs       # DEFLATE stream parsing
│   ├── tokens.rs       # LZ77 token types
│   └── tables.rs       # DEFLATE tables
├── bgzf/
│   ├── mod.rs          # BGZF format
│   ├── constants.rs    # BGZF constants (block sizes, etc.)
│   ├── writer.rs       # BGZF block writer
│   └── detector.rs     # BGZF format detection
└── transcoder/
    ├── mod.rs          # Transcoder traits
    ├── single.rs       # Single-threaded implementation
    ├── parallel.rs     # Multi-threaded implementation
    ├── boundary.rs     # Cross-boundary reference handling
    └── window.rs       # Sliding window for back-references
```

## Key Concepts

### Half-Decompression

The core insight is that we don't need to fully decompress data to re-compress it. Instead:

1. **Parse DEFLATE** to extract LZ77 tokens (literals + length/distance pairs)
2. **Track context** only for cross-boundary back-references
3. **Re-encode** tokens into new BGZF blocks

### LZ77 Tokens

```rust
pub enum LZ77Token {
    Literal(u8),                    // Raw byte
    Reference { length: u16, distance: u16 }, // Back-reference
}
```

### Cross-Boundary References

When a back-reference spans BGZF block boundaries, we must:
1. Detect the boundary crossing
2. Resolve the reference using our sliding window
3. Emit the data as literals in the new block

### Parallel Processing

Blocks are processed in parallel using a producer-consumer pipeline:
1. **Producer**: Parses input, extracts tokens, identifies block boundaries
2. **Consumers**: Encode blocks independently
3. **Writer**: Assembles blocks in order using sequence numbers

## Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run with output
cargo test -- --nocapture

# Run integration tests only
cargo test --test integration

# Run benchmarks
cargo bench
```

## CI Commands

The project uses cargo aliases for CI:

```bash
# Run tests (requires cargo-nextest)
cargo ci-test

# Run clippy linting
cargo ci-lint

# Check formatting
cargo ci-fmt
```

## Debugging

### Verbose Output

```bash
# CLI verbose mode
rebgzf -i input.gz -o output.bgz -v
```

### Environment Variables

```bash
# Enable debug logging (if log crate is added)
RUST_LOG=debug cargo run -- -i input.gz -o output.bgz
```

## Benchmarking

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench transcode

# Generate HTML report
cargo bench -- --save-baseline main
```

## Common Tasks

### Adding a New Feature

1. Create a feature branch
2. Implement the feature with tests
3. Update documentation if needed
4. Run the full test suite
5. Submit a PR

### Fixing a Bug

1. Write a failing test that reproduces the bug
2. Fix the bug
3. Verify the test passes
4. Check for regressions with full test suite

### Performance Work

1. Run benchmarks to establish baseline
2. Make changes
3. Run benchmarks again to measure improvement
4. Consider edge cases and pathological inputs
