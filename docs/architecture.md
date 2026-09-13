# Alloy Architecture & Runtime Internals

Alloy is architected from the ground up to replace the heavyweight V8 engine for modern systems programming.

```
+-------------------------------------------------------------------------+
|                              Alloy CLI                                  |
|                 (run, repl, bench, test, pkg, add, lsp)                 |
+-------------------------------------------------------------------------+
|                 Alloy Runtime (alloy-rt)                                |
|   Tokio Asynchronous Event Loop • Actor Scheduler • HTTP/1.1 Engine     |
+-------------------------------------------------------------------------+
|                 Alloy Virtual Machine (alloy-vm)                        |
|   AST Parser • Bytecode Compiler • Inline Caches • Cranelift JIT        |
+-------------------------------------------------------------------------+
|                 Alloy Core Primitives (alloy-core)                      |
|   NaN-Tagged 64-bit Values • Arena Allocator • Zero-Copy Shared Memory  |
+-------------------------------------------------------------------------+
|                              OS / Kernel                                |
|   Windows (CreateFileMappingW) • Linux/macOS (mmap / POSIX shm)         |
+-------------------------------------------------------------------------+
```

---

## 1. NaN-Tagged Value Representation

In 64-bit architectures, IEEE 754 floating-point numbers reserve a wide range of bit patterns for "Not a Number" (NaN). Any 64-bit value with bits 52–62 set to 1 and at least one bit in bits 0–51 set to 1 is a quiet NaN.

Alloy exploits this unused 51-bit payload space to store all primitive JavaScript values within a single `u64` register without heap allocations:

- **Float64**: Canonical IEEE-754 double (when not quiet-NaN).
- **Int32**: Small integers (Smi) encoded directly into lower 32 bits.
- **Booleans**: `true` and `false` encoded as dedicated bit flags.
- **Null & Undefined**: Distinct constant payloads.
- **Heap Pointers**: 48-bit canonical pointers addressing memory inside the Arena or Shared Memory segment.
- **Symbols**: Distinct 44-bit symbol IDs.

This design enables:
1. Complete elimination of pointer indirection for numbers, booleans, and small values.
2. Constant cache-line friendly registers.
3. Zero heap allocation during arithmetic operations.

---

## 2. Arena Allocator (The Deterministic GC)

Traditional JavaScript runtimes rely on complex tracing garbage collectors (like V8's Scavenger and Mark-Sweep-Compact), leading to unpredictable latency spikes and high memory overhead.

Alloy introduces an **Arena Allocation model**:
- Each isolated execution context or HTTP request receives a dedicated bump-pointer memory arena (`Arena`).
- Memory allocation is a simple pointer addition (`ptr += size`), completing in single-digit CPU clock cycles.
- When an ephemeral task completes (such as sending an HTTP response), the entire arena is detonated at once in $O(1)$ time by resetting the offset or releasing the block back to the OS.
- Generational mark-sweep GC is retained exclusively for persistent, long-lived background actors.

---

## 3. Bytecode Compiler & Dispatch Loop

Alloy bypasses the heavyweight AST-to-IR-to-Turbofan compilation tiers of V8:
1. **Lexer & Recursive-Descent Parser**: Produces a clean AST (`alloy-vm::compiler::ast`) with recursion depth limits preventing stack overflows.
2. **Bytecode Generator**: Emits dense, register-based opcodes (`Op::Add`, `Op::GetProp`, `Op::Call`, `Op::JumpIfFalse`).
3. **Optimized Dispatch**: The execution loop utilizes direct dispatch and thin Link-Time Optimization (LTO) in Rust to execute millions of bytecode instructions per second.

---

## 4. Inline Caches (IC)

Property lookups on dynamic JavaScript objects (`obj.prop`) are typically expensive due to hash-table hashing and prototype chain traversal.

Alloy implements **Inline Caches**:
- Each property access instruction maintains a cache slot recording the object shape ID and memory offset.
- **Monomorphic IC**: If the object shape matches the recorded shape, property access occurs via a single memory offset read ($O(1)$).
- **Polymorphic IC**: Handles up to 4 distinct shapes before gracefully degrading to hash lookup.

---

## 5. Cranelift Baseline JIT

For compute-heavy hot loops, Alloy integrates the **Cranelift** code generator:
- Execution loops track iteration frequency.
- When a loop threshold is exceeded (e.g., 10,000 iterations), the VM JIT-compiles the loop body to native x86_64 / aarch64 machine code in memory.
- The JIT execution transitions seamlessly back to bytecode upon exiting the loop.
