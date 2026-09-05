#!/usr/bin/env bash
# Speed comparison: alloy vs Node (V8).
#
# Usage:  bash difftest/bench.sh
#
# For each bench_*.js in difftest/bench:
#   1. verifies the two engines produce identical output,
#   2. times each engine (best of RUNS runs),
#   3. subtracts the engine's process-startup baseline,
#   4. prints alloy ms, node ms, and the alloy/node ratio.
#
# All times are wall-clock for the whole process (minus startup), so numbers
# include JIT warmup for Node and bytecode interpretation for alloy.
#
# Uses the release build by default (falls back to debug). The debug build is
# 10-50x slower — set ALLOY to override, e.g. ALLOY=./target/debug/alloy.exe.

set -u
BENCH_DIR="$(dirname "$0")/bench"
if [ -x ./target/release/alloy.exe ]; then
    ALLOY="${ALLOY:-./target/release/alloy.exe}"
else
    ALLOY="${ALLOY:-./target/debug/alloy.exe}"
fi
NODE="node"
RUNS="${BENCH_RUNS:-3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

nanos() { date +%s%N; }

# Startup baselines: an empty program in each engine.
printf 'print(0);\n' > "$WORK/empty.js"
printf 'const print = console.log;\n' > "$WORK/node_empty.js"

alloy_base=0
node_base=0
for _ in 1 2 3; do
    t0=$(nanos); "$ALLOY" "$WORK/empty.js" > /dev/null 2>&1; t1=$(nanos)
    alloy_base=$((alloy_base + (t1 - t0)))
    t0=$(nanos); "$NODE" "$WORK/node_empty.js" > /dev/null 2>&1; t1=$(nanos)
    node_base=$((node_base + (t1 - t0)))
done
alloy_base=$((alloy_base / RUNS))
node_base=$((node_base / RUNS))

best() { # best <cmd...> : run RUNS times, print best wall time in ns
    local best_ns=-1 t0 t1
    for _ in $(seq 1 "$RUNS"); do
        t0=$(nanos); "$@" > /dev/null 2>&1; t1=$(nanos)
        local dt=$((t1 - t0))
        if [ "$best_ns" -lt 0 ] || [ "$dt" -lt "$best_ns" ]; then
            best_ns=$dt
        fi
    done
    echo "$best_ns"
}

printf '%-22s %12s %12s %10s\n' "benchmark" "alloy (ms)" "node (ms)" "ratio"
printf '%s\n' "-------------------------------------------------------------------"
fail=0

for f in "$BENCH_DIR"/bench_*.js; do
    [ -f "$f" ] || continue
    name="$(basename "$f" .js)"

    # Output must match before timing is meaningful.
    "$ALLOY" "$f" > "$WORK/alloy.out" 2>&1 || { echo "DIFF/ERR $name (alloy failed)"; fail=1; continue; }
    { printf 'const print = console.log;\n'; cat "$f"; } > "$WORK/node_input.js"
    "$NODE" "$WORK/node_input.js" > "$WORK/node.out" 2>&1 || { echo "DIFF/ERR $name (node failed)"; fail=1; continue; }
    if ! diff -q "$WORK/alloy.out" "$WORK/node.out" > /dev/null 2>&1; then
        echo "DIFF/ERR $name (output mismatch)"
        diff "$WORK/alloy.out" "$WORK/node.out" | head -5
        fail=1
        continue
    fi

    a_best=$(best "$ALLOY" "$f")
    n_best=$(best "$NODE" "$WORK/node_input.js")

    # Subtract startup baseline; clamp at 1ms.
    a_ms=$(( (a_best - alloy_base) / 1000000 )); [ "$a_ms" -lt 1 ] && a_ms=1
    n_ms=$(( (n_best - node_base) / 1000000 )); [ "$n_ms" -lt 1 ] && n_ms=1
    ratio=$(awk "BEGIN { printf \"%.0f\", $a_ms / $n_ms }")

    printf '%-22s %12s %12s %10s\n' "$name" "$a_ms" "$n_ms" "${ratio}x"
done

echo
if [ "$fail" -eq 0 ]; then
    echo "All benchmark outputs matched between engines."
else
    echo "Some benchmarks errored or mismatched — see above."
fi
