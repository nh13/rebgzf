#!/bin/bash
# Generate flamegraphs for rebgzf transcoding
# Usage: ./scripts/profile.sh <input.gz> [threads]

set -e

INPUT="${1:-/Users/nhomer/work/clients/fulcrum/fgumi/inputs/SRR6109273_1.fastq.gz}"
THREADS="${2:-1}"
OUTPUT_DIR="profiles"

mkdir -p "$OUTPUT_DIR"

# Build release with debug symbols
echo "Building release with debug symbols..."
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release

# Option 1: samply (recommended for macOS)
if command -v samply &> /dev/null; then
    echo ""
    echo "=== Profiling with samply (${THREADS} thread(s)) ==="
    samply record -o "$OUTPUT_DIR/profile_${THREADS}t.json" \
        ./target/release/rebgzf -i "$INPUT" -o /tmp/out.bgzf -t "$THREADS"
    echo "Profile saved to: $OUTPUT_DIR/profile_${THREADS}t.json"
    echo "Open with: samply load $OUTPUT_DIR/profile_${THREADS}t.json"
fi

# Option 2: cargo-flamegraph (if installed)
if command -v flamegraph &> /dev/null; then
    echo ""
    echo "=== Generating flamegraph (${THREADS} thread(s)) ==="
    flamegraph -o "$OUTPUT_DIR/flamegraph_${THREADS}t.svg" -- \
        ./target/release/rebgzf -i "$INPUT" -o /tmp/out.bgzf -t "$THREADS"
    echo "Flamegraph saved to: $OUTPUT_DIR/flamegraph_${THREADS}t.svg"
fi

# Option 3: DTrace on macOS (no extra tools needed)
if [[ "$OSTYPE" == "darwin"* ]] && command -v dtrace &> /dev/null; then
    echo ""
    echo "=== Quick timing breakdown ==="
    # Just time the execution
    time ./target/release/rebgzf -i "$INPUT" -o /tmp/out.bgzf -t "$THREADS" -v
fi

echo ""
echo "Done! To compare 1T vs 4T:"
echo "  ./scripts/profile.sh '$INPUT' 1"
echo "  ./scripts/profile.sh '$INPUT' 4"
