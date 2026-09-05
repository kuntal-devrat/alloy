#!/usr/bin/env bash
# Fuzz differential: randomly-generated expressions over the full operator
# set, alloy vs Node (V8).
#
# Usage:  bash difftest/fuzz.sh [count]          (default 1500 per seed)
#         COUNT=3000 SEEDS="7 42 99" bash difftest/fuzz.sh
#
# For each seed, difftest/fuzz/gen.js generates `count` display-safe
# expressions (evaluating each in Node and filtering values that V8 and
# Rust print differently: |v| >= 1e18, tiny non-zero, -0), writes them as
# one `print(<expr>);` per line, and the two engines' outputs are diffed.
# NaN is kept (prints "NaN" in both); ±Infinity is normalized by sed
# (alloy prints inf/-inf, V8 prints Infinity/-Infinity) exactly like run.sh.

set -u
DIR="$(dirname "$0")"
ALLOY="${ALLOY:-./target/debug/alloy.exe}"
NODE="node"
COUNT="${1:-${COUNT:-1500}}"
SEEDS="${SEEDS:-1 2 3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

total=0
fail=0
for SEED in $SEEDS; do
    "$NODE" "$DIR/fuzz/gen.js" "$COUNT" "$SEED" "$WORK/cases" || { echo "generator failed (seed $SEED)"; exit 1; }

    "$ALLOY" "$WORK/cases.js" > "$WORK/alloy.out" 2>&1
    a_exit=$?
    # Display-only normalization: alloy prints `inf`/`-inf`, V8 prints
    # `Infinity`/`-Infinity`.
    sed -i -e 's/\b-inf\b/-Infinity/g; s/\binf\b/Infinity/g' "$WORK/alloy.out"

    { printf 'const print = console.log;\n'; cat "$WORK/cases.js"; } > "$WORK/node_in.js"
    "$NODE" "$WORK/node_in.js" > "$WORK/node.out" 2>&1
    n_exit=$?

    if [ "$a_exit" -ne 0 ] || [ "$n_exit" -ne 0 ]; then
        echo "seed $SEED: engine failure (alloy exit=$a_exit, node exit=$n_exit) — the generator produced invalid code or an engine crashed:"
        head -4 "$WORK/alloy.out"
        head -4 "$WORK/node.out"
        exit 1
    fi

    # The comparator (difftest/fuzz/cmp.js) classifies each line: identical
    # strings match; a `**`-containing expression whose two values differ by
    # <= 2 ulps is *tolerated* (the ES spec allows Number::exponentiate to be
    # implementation-approximated, and V8's pow legitimately differs from a
    # correctly-rounded powf by 1 ulp on near-ties like `9 ** 17`); anything
    # else is a real mismatch.
    if "$NODE" "$DIR/fuzz/cmp.js" "$WORK/cases.txt" "$WORK/alloy.out" "$WORK/node.out" > "$WORK/cmp.out" 2> "$WORK/cmp.log"; then
        echo "seed $SEED: $COUNT expressions, all matched"
    else
        echo "seed $SEED: $COUNT expressions — $(grep -c '^MISMATCH' "$WORK/cmp.out") mismatched:"
        head -12 "$WORK/cmp.out"
        tail -1 "$WORK/cmp.log"
        fail=1
    fi
    total=$((total + COUNT))
done

echo
if [ "$fail" -eq 0 ]; then
    echo "=== $total fuzz expressions, all matched ==="
else
    echo "=== MISMATCHES FOUND ==="
    exit 1
fi
