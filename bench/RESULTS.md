# Alloy vs Node.js Benchmark — 2026-09-06

Environment: Windows x64, Node v26.4.0 (`C:\Program Files\nodejs\node.exe`, 98.45 MB),
alloy 0.1.0 release (`target/release/alloy.exe`, 1.60 MB, `codegen-units=1 thin-LTO`,
built 2026-09-06). CPU: same machine, sequential runs.

Methodology (mirrors `difftest/bench.sh` + `bench_timed.sh`):
- **Process-wall**: best-of-3 wall time per file (`time.perf_counter` around subprocess).
  Node wrapper prepends `const print = console.log;`. Startup baseline (empty `print(0)`,
  best-of-5) subtracted for "net" column, clamped ≥0.5 ms.
- **Pure compute**: body wrapped in `function __run(){...}`, called K times in one process,
  elapsed via engine's own `Date.now()`. `ALLOY_VM_BUDGET=10000000000` (unlimited) so the
  50M default instruction cap never throttles multi-K runs.
- **Correctness gate**: outputs must byte-match before timing counts. **24/24 matched.**

## 1. Process-wall (best-of-3, ms) — what a user feels per `alloy file` / `node file`

Baseline: alloy 9.6 ms, node 60.7 ms (best-of-5 empty program; med alloy 15.9 ms).
Node pays ~60 ms V8 bootstrap every invocation; alloy pays ~10 ms process spawn.

| benchmark | alloy | node | raw ratio | net (−base) | net ratio | status |
|---|---:|---:|---:|---:|---:|:---:|
| array.ajs | 66.3 | 74.5 | 0.9x | 56.7 / 13.8 | 4.1x | OK |
| cmp_chain.ajs | 357.8 | 73.1 | 4.9x | 348.2 / 12.5 | 27.9x | OK |
| correctness.ajs | 16.8 | 76.8 | 0.2x | 7.1 / 16.2 | 0.4x | OK |
| fib.ajs (fib 28) | 127.6 | 69.2 | 1.8x | 118.0 / 8.5 | 13.9x | OK |
| int_loop.ajs | 223.2 | 80.1 | 2.8x | 213.5 / 19.4 | 11.0x | OK |
| json.ajs | 277.2 | 89.6 | 3.1x | 267.6 / 29.0 | 9.2x | OK |
| map_growth.ajs | 82.7 | 77.1 | 1.1x | 73.0 / 16.4 | 4.4x | OK |
| mapset.ajs | 257.9 | 102.5 | 2.5x | 248.3 / 41.9 | 5.9x | OK |
| props.ajs | 238.4 | 92.0 | 2.6x | 228.8 / 31.3 | 7.3x | OK |
| strings.ajs | 21.0 | 72.8 | 0.3x | 11.3 / 12.1 | 0.9x | OK |
| bench_ackermann | 31.4 | 70.3 | 0.4x | 21.8 / 9.7 | 2.2x | OK |
| bench_array | 716.9 | 88.8 | 8.1x | 707.2 / 28.2 | 25.1x | OK |
| bench_cache | 212.7 | 95.8 | 2.2x | 203.1 / 35.1 | 5.8x | OK |
| bench_closure | 837.1 | 94.3 | 8.9x | 827.5 / 33.6 | 24.6x | OK |
| bench_collatz | 681.8 | 83.0 | 8.2x | 672.2 / 22.4 | 30.0x | OK |
| bench_fib (fib 30+memo) | 484.6 | 83.4 | 5.8x | 475.0 / 22.8 | 20.9x | OK |
| bench_loop | 394.8 | 87.3 | 4.5x | 385.2 / 26.7 | 14.5x | OK |
| bench_matrix | 101.3 | 73.7 | 1.4x | 91.7 / 13.0 | 7.1x | OK |
| bench_mixed | 33.2 | 72.9 | 0.5x | 23.6 / 12.2 | 1.9x | OK |
| bench_object | 320.0 | 84.0 | 3.8x | 310.3 / 23.3 | 13.3x | OK |
| bench_queens | 109.3 | 69.5 | 1.6x | 99.6 / 8.8 | 11.3x | OK |
| bench_sieve | 198.9 | 78.3 | 2.5x | 189.3 / 17.6 | 10.7x | OK |
| bench_sort | 232.1 | 75.4 | 3.1x | 222.4 / 14.8 | 15.1x | OK |
| bench_string | 19.8 | 64.7 | 0.3x | 10.2 / 4.1 | 2.5x | OK |

Raw <1.0x (alloy wins wall-clock) on tiny benches is **startup, not compute**:
`correctness` (0.2x), `strings` (0.3x), `bench_string` (0.3x), `ackermann` (0.4x),
`mixed` (0.5x), `array` (0.9x). Net column removes the baseline.

## 2. Pure compute (in-process `Date.now`, K calls, ms)

| benchmark | K | alloy | node | ratio |
|---|---:|---:|---:|---:|
| array.ajs | 3 | 151 | 21 | 7.2x |
| cmp_chain.ajs | 1 | 382 | 19 | 20.1x |
| correctness.ajs | 3 | 1 | 8 | 0.1x |
| fib.ajs | 1 | 181 | 9 | 20.1x |
| int_loop.ajs | 1 | 258 | 30 | 8.6x |
| json.ajs | 1 | 230 | 15 | 15.3x |
| map_growth.ajs | 3 | 232 | 45 | 5.2x |
| mapset.ajs | 1 | 233 | 34 | 6.9x |
| props.ajs | 1 | 254 | 14 | 18.1x |
| strings.ajs | 3 | 36 | 16 | 2.2x |
| bench_ackermann | 2 | 48 | 11 | 4.4x |
| bench_array | 1 | 761 | 21 | 36.2x |
| bench_cache | 1 | 239 | 37 | 6.5x |
| bench_closure | 1 | 759 | 30 | 25.3x |
| bench_collatz | 1 | 699 | 28 | 25.0x |
| bench_fib | 1 | 502 | 24 | 20.9x |
| bench_loop | 1 | 388 | 36 | 10.8x |
| bench_matrix | 3 | 315 | 12 | 26.2x |
| bench_mixed | 3 | 95 | 31 | 3.1x |
| bench_object | 1 | 323 | 22 | 14.7x |
| bench_queens | 1 | 100 | 9 | 11.1x |
| bench_sieve | 1 | 264 | 28 | 9.4x |
| bench_sort | 1 | 296 | 20 | 14.8x |
| bench_string | 3 | 29 | 13 | 2.2x |

Honest reading: V8 JIT is 2–36x faster on sustained compute. Closest gaps are
`correctness` (trivial), `strings`/`bench_string` (2.2x — rope+builder works),
`mixed` (3.1x). Widest gaps are closure-heavy (`bench_closure` 25x, `collatz` 25x,
`matrix` 26x, `bench_array` 36x — sort/map/filter closures + megamorphic IC misses).

## 3. Startup, size, compile

- Cold start (empty `print(0)`, 20 runs): alloy min 10.5 / med 15.9 / p90 19.4 ms;
  node best 60.7 ms → **~6x faster start**. Windows process spawn dominates both;
  VM-internal: `ALLOY_TRACE=1 bench/correctness.ajs` → compile 495 µs, total 2.07 ms.
- Binary: alloy 1.60 MB vs node 98.45 MB → **61x smaller**.
- PRD targets: `<2 ms Hello World` — not met wall-clock on Windows (10 ms floor is
  OS spawn), met VM-internal (2.07 ms total, 0.5 ms compile). `<5 MB idle` — plausible
  (1.6 MB binary + 393 KB stack + 1 MB shm) but not measured with peak-RSS here.
- `.ax`: `alloy --emit-ax bench/fib.ajs /tmp/fib.ax` → 127 bytes; `.ax` vs `.ajs`
  wall identical here (158 ms both — fib(28) dominates, load is noise).

## 4. Takeaways

- Ship for: CLI tools, edge cold starts, tiny scripts, string-heavy append loops,
  zero-copy polyglot (not timed here — needs Python sidecar bench).
- Don't ship for: hot loops / recursion / closures expecting JIT parity. Next wins:
  threaded dispatch, hidden-class IC hits on `bench_object`, array sort fast path,
  Map/Set inline caching (`mapset` 7x), JSON stringify fast path (`json` 15x).

Reproduce: `cargo build --release`, then `python bench_all.py` (wall) and
`ALLOY_VM_BUDGET=0 python bench_timed2.py` (compute). Scripts removed after run;
logic mirrors `difftest/bench.sh` / `bench_timed.sh`.
