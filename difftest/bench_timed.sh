#!/usr/bin/env bash
# In-process speed comparison: alloy vs Node (V8).
#
# Usage:  bash difftest/bench_timed.sh
#
# Unlike bench.sh (whole-process wall time minus startup), this harness wraps
# each bench body in a function, runs it BENCH_K times inside ONE process,
# and prints the elapsed ms measured by Date.now() inside the engine. Process
# startup cost therefore cannot corrupt the reading, and both engines measure
# pure compute. Outputs are still verified identical on a bare run first.
#
# BENCH_K:  how many times to execute the bench body per process (default 3)
# ALLOY:    override the alloy binary (default ./target/release/alloy.exe)

set -u
BENCH_DIR="$(dirname "$0")/bench"
if [ -x ./target/release/alloy.exe ]; then
    ALLOY="${ALLOY:-./target/release/alloy.exe}"
else
    ALLOY="${ALLOY:-./target/debug/alloy.exe}"
fi
NODE="node"
K="${BENCH_K:-3}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

printf '%-22s %12s %12s %10s\n' "benchmark" "alloy (ms)" "node (ms)" "ratio"
printf '%s\n' "---------------------------------------------------------------"
fail=0

for f in "$BENCH_DIR"/bench_*.js; do
    [ -f "$f" ] || continue
    name="$(basename "$f" .js)"

    # 1. Bare run: outputs must match before timing is meaningful.
    "$ALLOY" "$f" > "$WORK/alloy.out" 2>&1 || { echo "DIFF/ERR $name (alloy failed)"; fail=1; continue; }
    { printf 'const print = console.log;\n'; cat "$f"; } > "$WORK/node_input.js"
    "$NODE" "$WORK/node_input.js" > "$WORK/node.out" 2>&1 || { echo "DIFF/ERR $name (node failed)"; fail=1; continue; }
    if ! diff -q "$WORK/alloy.out" "$WORK/node.out" > /dev/null 2>&1; then
        echo "DIFF/ERR $name (output mismatch)"
        diff "$WORK/alloy.out" "$WORK/node.out" | head -5
        fail=1
        continue
    fi

    # 2. Timed runs: wrap the bench body in a function, call it K times,
    #    print the elapsed ms.  The engine's own Date.now() measures inside
    #    the process, so process startup never contaminates the reading.
    {
        printf 'let __t0 = Date.now();\n'
        printf 'function __run() {\n'
        cat "$f"
        printf '}\n'
        for _ in $(seq 1 "$K"); do printf '__run();\n'; done
        printf 'print(Date.now() - __t0);\n'
    } > "$WORK/alloy_timed.js"
    {
        printf 'const print = console.log;\n'
        printf 'let __t0 = Date.now();\n'
        printf 'function __run() {\n'
        cat "$f"
        printf '}\n'
        for _ in $(seq 1 "$K"); do printf '__run();\n'; done
        printf 'print(Date.now() - __t0);\n'
    } > "$WORK/node_timed.js"

    a_ms=$("$ALLOY" "$WORK/alloy_timed.js" 2>/dev/null | tail -1 | tr -cd '0-9')
    n_ms=$("$NODE" "$WORK/node_timed.js" 2>/dev/null | tail -1 | tr -cd '0-9')
    a_ms="${a_ms:-0}"; n_ms="${n_ms:-0}"
    [ "$n_ms" -lt 1 ] && n_ms=1
    ratio=$(awk "BEGIN { printf \"%.1f\", $a_ms / $n_ms }")

    printf '%-22s %12s %12s %10s\n' "$name" "$a_ms" "$n_ms" "${ratio}x"
done

echo
if [ "$fail" -eq 0 ]; then
    echo "All benchmark outputs matched between engines."
else
    echo "Some benchmarks errored or mismatched — see above."
fi
