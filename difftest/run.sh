#!/usr/bin/env bash
# Differential test: alloy vs Node (V8).
#
# Usage:  bash difftest/run.sh [corpus-dir]
#
# Runs every t*.js in the corpus through both engines, normalizes a few
# display-only differences (inf/Infinity, -0), and diffs the output.
#
# The corpus is written in the intersection of the two languages:
#   * statements end with ';' (alloy has no ASI)
#   * no `==` with mixed types, no do-while, no comma operator, no block-scope
#     shadowing, no use-before-declaration, no numbers outside the range where
#     both engines print identically (|x| < 1e21 and >= 1e-6)
# Node gets `const print = console.log;` prepended; alloy has print built in.

set -u
DIR="${1:-$(dirname "$0")}"
if [ -z "${ALLOY:-}" ]; then
    if [ -f "./target/debug/alloy.exe" ]; then
        ALLOY="./target/debug/alloy.exe"
    else
        ALLOY="./target/debug/alloy"
    fi
fi
NODE="node"
TMPA="$(mktemp)"
TMPN="$(mktemp)"
WORK="$(mktemp -d)"
trap 'rm -f "$TMPA" "$TMPN"; rm -rf "$WORK"' EXIT

pass=0
fail=0
failed_files=()

for f in "$DIR"/t*.js; do
    [ -f "$f" ] || continue
    name="$(basename "$f")"

    "$ALLOY" "$f" > "$TMPA" 2>&1
    alloy_exit=$?
    # Display-only normalization: alloy prints `inf`/`-inf`, V8 prints
    # `Infinity`/`-Infinity`.
    sed -i -e 's/\b-inf\b/-Infinity/g; s/\binf\b/Infinity/g' "$TMPA"

    { printf 'const print = console.log;\n'; cat "$f"; } > "$WORK/node_input.js"
    "$NODE" "$WORK/node_input.js" > "$TMPN" 2>&1
    node_exit=$?
    # Display-only normalization: V8 prints `-0`, alloy prints `0`.
    sed -i 's/\b-0\b/0/g' "$TMPN"

    if [ "$alloy_exit" -ne 0 ] || [ "$node_exit" -ne 0 ]; then
        echo "DIFF  $name  (alloy exit=$alloy_exit, node exit=$node_exit)"
        failed_files+=("$name")
        fail=$((fail + 1))
        continue
    fi

    if diff -q "$TMPA" "$TMPN" > /dev/null 2>&1; then
        echo "PASS  $name"
        pass=$((pass + 1))
    else
        echo "DIFF  $name"
        failed_files+=("$name")
        fail=$((fail + 1))
    fi
done

echo
echo "=== $pass passed, $fail failed ==="
if [ "$fail" -gt 0 ]; then
    echo "Failed: ${failed_files[*]}"
    exit 1
fi
