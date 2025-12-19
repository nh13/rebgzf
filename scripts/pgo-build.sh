#!/bin/bash
# Profile-Guided Optimization (PGO) build script
# Usage: ./scripts/pgo-build.sh <training-file.gz>

set -e

TRAINING_FILE="${1:-/Users/nhomer/work/clients/fulcrum/fgumi/inputs/SRR6109273_1.fastq.gz}"
PGO_DIR="$(pwd)/target/pgo-profiles"

echo "=== PGO Build ==="
echo "Training file: $TRAINING_FILE"
echo ""

# Clean previous PGO data
rm -rf "$PGO_DIR"
mkdir -p "$PGO_DIR"

# Step 1: Build with instrumentation
echo "Step 1: Building with instrumentation..."
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" cargo build --release

# Step 2: Run instrumented binary to collect profile data
echo ""
echo "Step 2: Collecting profile data..."
./target/release/rebgzf -i "$TRAINING_FILE" -o /tmp/pgo_out.bgzf -t 1
./target/release/rebgzf -i "$TRAINING_FILE" -o /tmp/pgo_out.bgzf -t 2

# Step 3: Merge profile data
echo ""
echo "Step 3: Merging profile data..."
# Find llvm-profdata (might be named differently on different systems)
LLVM_PROFDATA=$(which llvm-profdata 2>/dev/null || find /opt/homebrew -name 'llvm-profdata' 2>/dev/null | head -1 || echo "")
if [ -z "$LLVM_PROFDATA" ]; then
    # Try xcrun on macOS
    LLVM_PROFDATA=$(xcrun -f llvm-profdata 2>/dev/null || echo "")
fi

if [ -z "$LLVM_PROFDATA" ]; then
    echo "ERROR: llvm-profdata not found. Install LLVM or use Xcode."
    exit 1
fi

"$LLVM_PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"/*.profraw

# Step 4: Rebuild with profile data
echo ""
echo "Step 4: Rebuilding with profile data..."
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata -Cllvm-args=-pgo-warn-missing-function" cargo build --release

echo ""
echo "=== PGO build complete! ==="
echo "Binary: ./target/release/rebgzf"
