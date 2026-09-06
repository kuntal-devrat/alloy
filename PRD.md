# Product Requirements Document: alloy

## 1. Product Overview

**Product Name:** alloy
**One-Liner:** A hyper-optimized, polyglot systems runtime written in Rust that executes JavaScript, Python, C, and native concurrency-first languages within a single file using a shared memory segment, completely bypassing traditional V8 overhead and garbage collection.

**Vision:** To build a runtime that resurrects the syntax of JavaScript for modern systems engineering, strips away its historical browser baggage, and provides a zero-serialization bridge to AI ecosystems (Python) and low-level execution (Rust/C). It also serves as the foundational engine for new concurrency paradigms, such as message-passing languages like Pulse.

## 2. The Problem Space

1. **The V8 Bloat & JIT Overhead:** V8 was built to render complex web applications. It consumes massive amounts of RAM just to boot its Just-In-Time (JIT) compiler pipeline. Edge computing and microservices require microsecond boot times, not millisecond warm-ups.
2. **The FFI Serialization Bottleneck:** Modern development requires mixing languages (e.g., JS for web routing, Python for machine learning tensors). Current Foreign Function Interfaces (FFI) require expensive serialization/deserialization (JSON/Buffers) over sockets or pipes, crippling performance.
3. **The Shared State Trap:** JavaScript is fundamentally single-threaded. Scaling it across CPU cores relies on `worker_threads` and `SharedArrayBuffer`, leading to race conditions, deadlocks, and complex mutex management.
4. **Garbage Collection Pauses:** V8's tracing garbage collector periodically stops execution to clean up memory, causing unpredictable latency spikes in high-performance environments.

## 3. Target Audience

* **Systems Engineers & Tooling Creators:** Developers building high-performance CLI tools, bundlers, and local development servers.
* **AI/ML Orchestrators:** Engineers who want to write lightweight JavaScript API layers that directly manipulate Python AI models in memory without microservice overhead.
* **Edge Compute Developers:** Teams deploying serverless functions that require instantaneous cold starts.

## 4. Core Architectural Pillars

### 4.1. The Chassis (Rust Core)

* **Requirement:** The entire runtime must be written in Rust to ensure absolute memory safety, fearless concurrency, and an ultra-lean binary size.
* **Functionality:** Replaces `libuv` and V8's C++ core with a bespoke, asynchronous event loop built on Rust's `tokio` or `mio`, handling OS-level I/O with minimal overhead.

### 4.2. Custom Bytecode VM (AOT/JIT Hybrid)

* **Requirement:** Skip the heavy AST-to-JIT pipeline of standard engines.
* **Functionality:** A custom, register-based Virtual Machine. When a script runs, alloy instantly compiles the JS down to lean bytecode and executes it directly. For long-running processes, a highly selective optimizer can step in, but the priority is absolute zero-latency execution on startup.

### 4.3. Arena Memory Allocator (The GC Killer)

* **Requirement:** Eliminate the unpredictable pauses of tracing garbage collection.
* **Functionality:** Implements an Arena Allocator. When an isolated process spins up, it is handed a fixed block of memory (an arena). The runtime does not track individual object lifecycles. When the process completes its task (e.g., returning an HTTP response), the engine issues a single C-level `free()` command, detonating the entire arena instantly.

### 4.4. Sidecar Memory Architecture (Zero-Copy Polyglot)

* **Requirement:** Allow Python, JavaScript, C, and Rust to execute seamlessly in the same file.
* **Functionality:** Allocates a shared memory segment at the system level. The embedded JS bytecode interpreter and a bound CPython interpreter are given raw pointers to this exact same memory block. A JavaScript array modification is instantly readable as a NumPy tensor by the Python execution context. Zero serialization, zero network overhead.

### 4.5. Message-Passing Concurrency Model

* **Requirement:** Native concurrency based on isolated memory spaces, avoiding shared memory threads.
* **Functionality:** Every execution context is strictly isolated. To scale across cores, the runtime provides native message-passing primitives built into the event loop. This enables Erlang/Go-style concurrency, acting as the perfect compilation target for concurrency-first languages like Pulse, where isolated agents communicate asynchronously without race conditions.

## 5. Developer Experience (DX) & API

The developer experience must feel like magic: writing multiple languages in a single routing file with zero configuration.

**Source extension:** alloy source files use the native extension `.ajs` (a uniquely-alloy extension, so a `.ajs` file can never be mistaken for a browser script). Plain `.js` remains accepted for compatibility, and `.ax` is the precompiled bytecode format emitted by `alloy --emit-ax`.

```javascript
// server.ajs
import { http, memory } from 'alloy:core';
import { processTensor } from './ai_model.py' as python;

http.createServer(async (req, res) => {
  // 1. Allocate a chunk in the Sidecar Memory Segment
  const sharedBuffer = memory.allocateFloat32Array(req.body.imagePixels);
  
  // 2. Call Python directly. No JSON parsing, no local HTTP requests.
  // Python reads the exact C-pointer address instantly.
  const classification = await python.processTensor(sharedBuffer.ptr);
  
  // 3. Respond. The Arena Allocator detonates all local scope memory instantly.
  res.send({ label: classification });
}).listen(8080);
```

### 5.1. Operational Natives

The runtime seeds a small set of global natives for operators, in addition to the
standard library (`print`, `http`, `memory`, `fs`, `Promise`, `setTimeout`,
`channel`, `spawn`, `require`, `reload`, `Date`, `Math`, `JSON`, `Number`,
`fetchSync`, `crypto`, `URL`, `encodeURIComponent`, `btoa`, …).

### 5.2. Web Surface (API-shaped apps)

`http.createServer(handler).listen(port)` serves JSON APIs and static frontends
(`examples/web-todos` is the reference app: router + HS256 JWT + JSON-file DB).
Handlers receive `req = { method, url, path, query, headers, cookies, body,
protocol, ip }` (`X-Forwarded-Proto/For` trusted for TLS-terminating proxies)
and answer with `res.{send, json, text, html, status(code), set(k, v)}`
(`send(string)` is raw, Express-style; `json` is the explicit JSON path).
Outgoing calls use `fetchSync(url, { method, headers, body, timeoutMs })`
(http:// only — https refuses loudly; run concurrent fetches inside `spawn`
workers). `crypto` ships dependency-free SHA-256/HMAC-SHA256/base64(randomHex,
timingSafeEqual) so JWT auth needs no packages. Plain HTTP only — terminate
TLS at Caddy/nginx (`examples/web-todos/Caddyfile`).

**`sweepSegments()` — reclaim shared-memory segments leaked by crashed runs.**

The shared-memory segment backing each VM is a temp file
(`alloy_shm_{pid}_{…}.tmp`) that is deleted when the VM drops cleanly. A
crashed or killed run never drops, so its file lingers. Every process sweeps
orphans once at startup (rate-limited to once per 60s, reporting
`[alloy] reclaimed N orphaned shared-segment file(s) … (B bytes total)` when
anything is found); `sweepSegments()` runs that same sweep on demand, so a
long-running server can reclaim crashed-run segments between requests instead
of waiting for the next process start.

```javascript
// Long-running server: reclaim + report between requests.
const r = sweepSegments();
if (r.files > 0) {
  log(`reclaimed ${r.bytes} bytes from ${r.files} leaked segments`);
}
```

* **Returns** `{ files, bytes }` — how many segment files were reclaimed and
their total size (each file is one crashed run's segment).
* **Safety:** identical to the startup sweep — a live process's segment is
never touched (pid-liveness on Unix; open-file delete semantics on Windows),
and files younger than a 60-second grace period are kept, so calling it
freely is safe.
* **Host API:** the same result is available to Rust hosts as
`alloy_core::shared_memory::last_orphan_sweep()` (the most recent sweep's
`{ files, bytes }`) for monitoring integrations.

## 6. Technical Constraints & Security

* **Pointer Safety Boundaries:** Allowing a JS context to share memory with C/Python introduces massive segmentation fault risks if boundaries are exceeded. The Rust core must enforce strict bounds checking on the shared memory block before the C/Python interpreters can access it.
* **Standard Library:** alloy cannot use Node.js packages that rely on V8 C++ bindings (e.g., `node-gyp`). A new, lean standard library must be established for file I/O, networking, and cryptography.
* **Context Switching:** The overhead of context switching between the JS VM and the Python VM inside the shared memory must be kept under 5 microseconds.

## 7. Success Metrics

* **Cold Start Time:** Boot and execute a "Hello World" JS script in `< 2 milliseconds` (compared to Node's ~30-50ms).
* **Polyglot Execution:** Passing a 10MB array from JavaScript to Python must execute in `O(1)` time (instantaneous pointer handoff), compared to `O(N)` stringification.
* **Memory Footprint:** The idle runtime process should consume `< 5MB` of RAM.
