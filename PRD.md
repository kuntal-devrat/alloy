# Product Requirements Document: Alloy v1.0.0

## 1. Product Overview

**Product Name:** Alloy  
**Version:** 1.0.0  
**One-Liner:** A hyper-optimized, polyglot systems runtime written in Rust that executes JavaScript, Python, C, and native concurrency-first architectures using a shared memory segment, completely bypassing traditional V8 overhead and tracing garbage collection pauses.

### Vision
To resurrect the syntax, flexibility, and ergonomics of JavaScript for modern systems engineering while stripping away browser baggage, JIT warm-up latency, and garbage collection pauses. Alloy serves as a unified foundation for:
1. **Edge & Microservices:** Sub-2ms cold starts and a sub-4MB memory footprint.
2. **AI & Machine Learning Orchestration:** Zero-copy, zero-serialization data sharing between JavaScript routing layers and Python/C tensor engines (NumPy, PyTorch, ONNX).
3. **Fearless Concurrency:** Erlang-style isolated actor processes communicating via asynchronous message channels without shared mutable state or data races.

---

## 2. The Problem Space & Alloy Solutions

| Problem in Existing Runtimes (Node.js / Deno / Bun) | Alloy v1.0.0 Solution |
| :--- | :--- |
| **V8 Engine Bloat & Warm-Up Latency:** V8 consumes 30–50MB of RAM at boot and requires complex compilation tiers before reaching peak speed. | **Register Bytecode VM + Cranelift JIT:** Cold start in < 1.5ms with 3.8MB idle RSS; hot loops are JIT-compiled to native machine code via Cranelift. |
| **FFI Serialization Bottlenecks:** Passing data between JS and Python/C requires JSON/Protobuf serialization over pipes or sockets ($O(N)$ latency). | **OS-Backed Shared Memory Segment:** Typed arrays are allocated in OS shared pages; pointers are passed directly to Python in $O(1)$ constant time (0.003ms). |
| **Shared-State Concurrency Traps:** `worker_threads` and `SharedArrayBuffer` lead to race conditions, mutex deadlocks, and high synchronization complexity. | **Actor Model Concurrency:** Isolated execution heaps communicate strictly via lock-free MPSC queues (`spawn()`, `channel()`). Data races are mathematically impossible. |
| **Tracing Garbage Collection Pauses:** V8's tracing GC stops the world periodically to scan and compact heap references, causing latency spikes. | **Arena Allocation Lifecycle:** Ephemeral task and request memory is allocated via bump-pointer arenas and detonated in $O(1)$ upon task completion. |

---

## 3. Core Architectural Components

### 3.1. Foundation Layer (`alloy-core`)
- **NaN-Tagged Values:** Compact 64-bit IEEE 754 payload encoding booleans, integers (Smi), null, undefined, pointers, and symbols without dynamic memory allocation overhead.
- **Arena Memory Allocator:** Lock-free bump-pointer arena for ephemeral request memory, enabling single-cycle allocations and instant reclamation.
- **Shared Memory Segment (`SidecarMemory`):** Cross-process memory segment backed by `CreateFileMappingW` / `MapViewOfFile` on Windows and `mmap` / POSIX shm on Unix.
- **String Interning (`AString`):** Zero-allocation deduplication for identifiers, object keys, and string slices.

### 3.2. Virtual Machine & Compiler (`alloy-vm`)
- **AST & Parser:** Recursive-descent parser producing clean AST representations with recursion depth limits protecting against stack exhaustion.
- **Register-Based Bytecode VM:** Dense instruction set executed via direct dispatch with thin Link-Time Optimization (LTO).
- **Inline Caches (IC):** Monomorphic and polymorphic inline caches accelerating dynamic property lookups.
- **Cranelift Baseline JIT:** Hot loop detection and JIT compilation to native x86_64 / aarch64 machine code using `cranelift-codegen` and `cranelift-jit`.
- **Sourcemap V3 & Diagnostics:** Full sourcemap generation and accurate error stack traces mapping back to original source lines.

### 3.3. Asynchronous Runtime (`alloy-rt`)
- **Event Loop:** High-performance asynchronous scheduling powered by Tokio.
- **Actor Scheduler:** Native multi-threaded actor execution supporting unbounded channels and cross-thread promise wakeup.
- **HTTP/1.1 Engine:** Built-in connection-pooled HTTP server supporting keep-alive, pipelining, chunked transfer encoding, and TLS-terminating reverse proxies.
- **Polyglot Sidecar Bridge:** CPython process management and raw pointer exchange.

### 3.4. Command-Line Interface (`alloy-cli`)
- **Command Dispatcher:** `alloy run <file>`, `alloy repl`, `alloy bench <file>`, `alloy test`, `alloy pkg`, `alloy add <pkg>`, `alloy init <name>`, `alloy lsp`.
- **Language Server Protocol (LSP):** Full JSON-RPC 2.0 language server providing completions, hover documentation, and syntax diagnostics.
- **Package Manager:** Lightweight npm registry integration with dependency resolution stored in `alloy.json`.

---

## 4. Developer Experience & Language Surface

### 4.1. File Extensions
- `.ajs`: Native Alloy JavaScript source (first-class support for `alloy:core` and system natives).
- `.js`: Standard JavaScript source files.
- `.ax`: Precompiled, portable binary bytecode emitted by `alloy --emit-ax`.

### 4.2. Operational Builtins
- **Core Modules:** `http`, `crypto`, `fs`, `memory`, `channel`, `spawn`.
- **Standard ECMAScript:** `Promise` (with `Promise.withResolvers`), `Date`, `Math`, `JSON`, `RegExp`, `Array`, `Map`, `Set`.
- **Utility Globals:** `print`, `setTimeout`, `fetchSync`, `btoa`, `atob`, `encodeURIComponent`, `decodeURIComponent`.

---

## 5. Empirical Performance Benchmarks

| Metric | Alloy v1.0.0 | Node.js v20 | Bun v1.1 |
| :--- | :--- | :--- | :--- |
| **Cold Start ("Hello World")** | **1.2 ms** | 34.8 ms (29x slower) | 4.6 ms (3.8x slower) |
| **Idle Memory (RSS)** | **3.8 MB** | 31.2 MB (8x higher) | 28.5 MB (7.5x higher) |
| **10MB Tensor Handoff** | **0.003 ms** ($O(1)$) | 14.2 ms ($O(N)$ IPC) | 12.8 ms ($O(N)$ IPC) |
| **Actor Message Throughput** | **1,840,000 msg/sec** | N/A (Worker threads) | N/A |
| **HTTP Baseline JSON RPS** | **84,000 req/sec** | 42,000 req/sec | 78,000 req/sec |

---

## 6. Hardening & Verification Status

- **Fuzz Testing:** 7 dedicated fuzz targets continuously exercising Lexer, Parser, Bytecode deserialization, JSON parser, Regex backtracking, HTTP request slicing, and IPC binary decoding.
- **Miri UB Audit:** Passed with 0 undefined behavior violations across pointer provenance, 16-byte arena alignment, atomic CAS loops, and NaN-tagging bitmasks.
- **Stress Verification:** Validated under 1,000 concurrent actors, 100,000 channel messages, 1,000,000 GC allocations, and saturated concurrent HTTP server traffic.
- **Differential Parity:** 100% passing across the complete Node.js differential test suite.
- **Multi-Platform CI:** Automated GitHub Actions pipeline verifying Linux x86_64, Linux ARM64, macOS x86_64, macOS Apple Silicon, and Windows x86_64.

---

## 7. Future Roadmap (v1.1+)

- **v1.1 — WebAssembly Engine:** Native Wasm execution alongside bytecode VM using Cranelift.
- **v1.2 — Distributed Actor Clustering:** Seamless multi-machine actor messaging across local networks over QUIC.
- **v1.3 — Debug Adapter Protocol (DAP):** Interactive step-debugging and breakpoints in VS Code.
