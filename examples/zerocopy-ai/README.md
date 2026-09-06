# zerocopy-ai — JS API + Python AI in one file, zero-copy

A tiny full-stack demo for **alloy** (a Rust JS runtime): an HTTP API written in
JavaScript that calls a Python classifier living in the **same shared-memory
segment**. No JSON bridge, no sockets, no copies between the two languages —
JS writes floats once, Python reads the exact bytes.

Same algorithm runs on Node via the classic path (JSON + spawn-per-request),
so the benchmark measures **only the bridge**.

```
examples/zerocopy-ai/
  server.ajs       # alloy: 30 lines — http server + memory.allocateFloat32Array + await python.classify
  model.py         # alloy side: reads shared segment via read_bytes (stdlib only)
  server.node.js   # node baseline: same API, execFileSync(model_cli.py) per request
  model_cli.py     # node side: identical math, JSON-in/JSON-out
  handoff.ajs / handoff.node.js   # pure bridge microbench (no HTTP)
  bench.py         # orchestrates both servers, checks labels, prints latency
```

## 60-second demo

```sh
cargo build --release
python examples/zerocopy-ai/model_cli.py <<< '{"pixels":[1,2,3]}'
node examples/zerocopy-ai/server.node.js &   # :8082
ALLOY_VM_BUDGET=0 ALLOY_SHM_CAP=16777216 ./target/release/alloy examples/zerocopy-ai/server.ajs &  # :8081
curl -X POST localhost:8081/classify -d '{"pixels":[0.5,-0.2,0.9]}'
# {"label":"bright 0.71.."}
```

## Measured (Windows x64, Node v26.4.0, alloy 0.1.0, 1024-float payload)

| test | alloy | node | speedup |
|---|---|---:|---:|
| bridge only — 20× `classify`, no HTTP (`handoff.ajs` vs `handoff.node.js`) | 54 ms | 865 ms | **16×** (2.7 vs 43 ms/call) |
| end-to-end — 100× `POST /classify` (`bench.py`) | 24.0 mean / 25.5 p50 / 28.0 p95 ms | 53.2 / 53.1 / 64.1 ms | **2.2×** |
| cold start (empty `print(0)`) | ~6 ms | ~41 ms | **7×** |
| binary | 1.6 MB | 98 MB | **61× smaller** |

Labels byte-agree on every request (`mid 0.6812`). At 16k floats both converge
(~1.0×) — Python-side `struct.unpack` dominates and the bridge stops mattering.
Honest framing: alloy removes the *spawn+serialize* tax; it doesn't speed up
your model math. With numpy/`ALLOY_PYTHON_EMBED=1` the gap widens again.

## Why it works

```
alloy:  JS --write f32--> [ shared mmap segment ] <--read_bytes-- Python (persistent worker)
node:   JS --JSON string--> spawn python per request --> parse stdout (per-request fork tax)
```

The sidecar maps the same file (`alloy_shm_*.tmp`), so `buf.ptr` is a segment
offset on both sides. Calls are a one-line control channel; data never crosses it.

## Try variations

- `python bench.py --n 16384 --reqs 20` — watch the gap shrink as compute dominates
- `ALLOY_JIT_LOG=1` / `ALLOY_OP_HIST=1` — see hot-loop + opcode profiles
- Swap `model.py` math for numpy (needs `ALLOY_PYTHON` with numpy) — bridge unchanged

---

<details><summary>Reddit draft (r/programming)</summary>

**I built a JS runtime in Rust where JS and Python share one memory segment — 16× faster AI handoff than Node**

`server.ajs` is 30 lines: JS HTTP handler allocates a Float32Array in shared
memory and `await python.classify(buf.ptr, buf.length)`. Python reads the exact
bytes — no JSON, no socket, no copy. Same algorithm on Node (spawn + JSON per
request) is 865ms vs 54ms for 20 calls, identical labels. End-to-end HTTP still
2.2× ahead; cold start 7×; binary 1.6MB vs 98MB.

Same-box numbers, stdlib-only model, repo includes both servers + bench script
so you can reproduce. Biggest lesson: the FFI serialization tax dwarfs everything
until your model math dominates (~16k floats here). Happy to answer architecture
questions (arena GC, NaN-boxing, sidecar pool).

</details>

<details><summary>LinkedIn draft</summary>

JavaScript for APIs, Python for AI — without the microservice in between.

I built zerocopy-ai on alloy, a Rust JS runtime: one `.ajs` file serves HTTP in
JS and calls Python through shared memory. The 1024-float vector is written once;
Python reads the same bytes. No serialization, no per-request process spawn.

Measured on one box: 16× faster bridge calls than Node's JSON+spawn path
(54ms vs 865ms / 20 calls), 2.2× end-to-end HTTP, 7× faster cold start, 61×
smaller binary — with byte-identical results.

The honest caveat is in the README: past ~16k floats, model compute dominates
and both converge. Kill the bridge tax where it matters (edge inference,
high-QPS small-payload scoring), keep your Python stack untouched.

Code + repro script in the repo. What polyglot bottleneck would you point this at?

</details>
