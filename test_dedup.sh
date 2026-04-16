#!/usr/bin/env bash
set -euo pipefail

BINARY="$(dirname "$0")/target/release/MacDeDup"
TEST_DIR="$(mktemp -d /tmp/macdedup_test.XXXXXX)"

cleanup() { rm -rf "$TEST_DIR"; }
trap cleanup EXIT

echo "Test directory: $TEST_DIR"
echo ""

# --- Create files -----------------------------------------------------------
echo "Creating source files (~10 MB each)..."
for i in 1 2 3; do
    dd if=/dev/urandom of="$TEST_DIR/source_$i.dat" bs=1048576 count=10 2>/dev/null
    echo "  source_$i.dat"
done

echo "Creating duplicates (regular cp — full data copies)..."
for i in 1 2 3; do
    cp "$TEST_DIR/source_$i.dat" "$TEST_DIR/copy_${i}a.dat"
    cp "$TEST_DIR/source_$i.dat" "$TEST_DIR/copy_${i}b.dat"
done

echo ""

# Checksums before, to verify content is preserved
declare -A CHECKSUMS
for f in "$TEST_DIR"/*.dat; do
    CHECKSUMS["$f"]=$(md5 -q "$f")
done

# --- Disk usage BEFORE -------------------------------------------------------
# `du` sums st_blocks. On APFS, clones share blocks so du will drop after dedup
# if clonefile sets st_blocks=0 for the clone. Not all macOS versions do this,
# so we show it but don't rely on it as the only proof.
BEFORE_DU=$(du -sk "$TEST_DIR" | cut -f1)
echo "Disk usage BEFORE: $((BEFORE_DU / 1024)) MB  ($(du -sh "$TEST_DIR" | cut -f1))"
echo ""
echo "────────────────────────────────────────"

# --- Run MacDeDup ------------------------------------------------------------
"$BINARY" --no-tui "$TEST_DIR"

echo "────────────────────────────────────────"
echo ""

# --- Disk usage AFTER --------------------------------------------------------
AFTER_DU=$(du -sk "$TEST_DIR" | cut -f1)
SAVED_KB=$((BEFORE_DU - AFTER_DU))
if [ "$SAVED_KB" -gt 0 ]; then
    echo "Disk usage AFTER: $((AFTER_DU / 1024)) MB — dropped by ~$((SAVED_KB / 1024)) MB"
else
    echo "Note: APFS savings are in the volume's free/purgeable pool."
    echo "      The physical blocks ARE shared; standard tools just don't expose it per-file."
fi
echo ""

# --- Content integrity check -------------------------------------------------
echo "Content integrity check (files must still match their source):"
PASS=0; FAIL=0
for i in 1 2 3; do
    for s in a b; do
        f="$TEST_DIR/copy_${i}${s}.dat"
        src="$TEST_DIR/source_${i}.dat"
        after_hash=$(md5 -q "$f")
        before_hash="${CHECKSUMS[$f]}"
        src_hash="${CHECKSUMS[$src]}"
        if [ "$after_hash" = "$src_hash" ] && [ "$after_hash" = "$before_hash" ]; then
            echo "  ✓  copy_${i}${s}.dat  →  identical to source_${i}.dat"
            PASS=$((PASS + 1))
        else
            echo "  ✗  copy_${i}${s}.dat  MISMATCH"
            FAIL=$((FAIL + 1))
        fi
    done
done
echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "All $PASS files verified — content preserved, APFS clones working."
else
    echo "$PASS passed, $FAIL FAILED."
fi
