use crate::bytecode::Program;
use crate::compiler::Compiler;
use crate::opcode::Opcode;
use crate::python_sidecar::{PyArg, PythonSidecar};
use alloy_core::arena::ChunkedArena;
use alloy_core::heap::{ArenaHeap, HeapGuard, KIND_ARRAY, KIND_OBJECT, PromoteMap};
use alloy_core::regex::{self, RegexCompiled};
use alloy_core::shared_memory::{SharedMemoryError, SidecarMemory};
use alloy_core::value::{
    ArrayData, ChannelItem, ChannelState, FunctionData, MarkState, ObjectData, PromiseState,
    PromiseStatus, RcDirtyRef, RegexState, Value, VmHost, WakeHandle, sweep_old_mark_sweep,
    sweep_young, js_number_to_string, to_string_js, walk_cell, walk_container_entries, walk_value,
};
use hashbrown::HashMap;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

// TEMP profiling: ALLOY_OP_HIST=1 enables an opcode execution histogram.
static OP_HIST: std::sync::OnceLock<Option<std::sync::Mutex<[u64; 256]>>> =
    std::sync::OnceLock::new();
fn op_hist() -> Option<&'static std::sync::Mutex<[u64; 256]>> {
    OP_HIST
        .get_or_init(|| {
            if std::env::var("ALLOY_OP_HIST").is_ok() {
                Some(std::sync::Mutex::new([0u64; 256]))
            } else {
                None
            }
        })
        .as_ref()
}

/// Fixed capacity of the operand stack (slots). Locals live on the same
/// stack, one slot each, and recursion is capped by `MAX_CALL_DEPTH`, so
/// realistic programs never approach this. Sized to keep idle memory under
/// the PRD's 5MB budget (16K x 24B ≈ 393KB): 512 frames at 32 slots each
/// fit exactly, and deeper/fatter frames fail gracefully via the call-entry
/// guard in `dispatch_call`.
/// Read through a live-import cell to its current value; non-cells pass
/// through unchanged. Used at every boundary where a cell could escape:
/// global loads, object property reads, JSON serialization.
#[inline]
fn unwrap_cell(v: Value) -> Value {
    match v.as_cell() {
        Some(c) => {
            // try_borrow first: a cell mutably borrowed by an in-flight
            // setter (or a native mid-write) must not panic a concurrent
            // read; fall back to the blocking borrow in that rare case.
            if let Ok(g) = c.try_borrow() {
                g.clone()
            } else {
                c.borrow().clone()
            }
        }
        None => v,
    }
}

const STACK_SIZE: usize = 16384;
/// Initial old-generation churn threshold for the second-generation sweep.
const MAJOR_THRESHOLD_INIT: usize = 1 << 20;
/// The adaptive threshold is clamped to this range (64KiB..16MiB).
const MAJOR_THRESHOLD_MIN: usize = 1 << 16;
const MAJOR_THRESHOLD_MAX: usize = 1 << 24;
/// Per-frame stack budget enforced at call entry: a frame whose base would
/// land within this many slots of the top fails gracefully instead of
/// overflowing the fixed stack.
const FRAME_BUDGET: usize = 512;
const SHARED_MEMORY_CAPACITY: usize = 1 << 20;
/// Cap on the process-wide compiled-module cache (`SharedModuleRegistry`).
/// Past this many unique module paths, older entries are flushed (a later
/// require recompiles) instead of growing without bound.
const MAX_COMPILED_MODULES: usize = 4096;
/// Cap for the per-path `.py` reload-state tracking (`SharedPyRegistry`),
/// mirroring `MAX_COMPILED_MODULES` for the python side.
const MAX_PY_TRACKED_MODULES: usize = 4096;

/// The shared segment's capacity in bytes: 1 MiB by default (the PRD's idle
/// memory footprint claim), overridable with `ALLOY_SHM_CAP` (raw bytes) so
/// a workload can hand off bigger buffers — e.g. the PRD's 10MB array demo:
/// `ALLOY_SHM_CAP=16777216`. Both python backends size their view (child
/// mmap / embed ctypes buffer) from the same value.
fn shared_memory_capacity() -> usize {
    std::env::var("ALLOY_SHM_CAP")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(SHARED_MEMORY_CAPACITY)
        .max(SHARED_MEMORY_CAPACITY)
}
const MAX_CALL_DEPTH: usize = 512;
/// Maximum number of child processes per imported `.py` file. Children spawn
/// lazily — the first at import time, more only when every existing child has
/// an in-flight call — so an idle server pays for exactly one python process
/// instead of K. Override with `ALLOY_PYTHON_POOL`.
const PYTHON_POOL_SIZE: usize = 2;
/// Per-call deadline for python sidecar calls: a function that never returns
/// kills its child and rejects the promise after this long instead of
/// parking the request forever. Override with `ALLOY_PYTHON_TIMEOUT_MS`.
const PYTHON_CALL_TIMEOUT_MS: u64 = 10_000;
/// Size of the direct-mapped monomorphic inline cache for GetProperty /
/// SetProperty. Slots are indexed by `pc & (IC_SLOTS - 1)`; hot loops are
/// small, so 256 entries rarely collide, and a collision is just a miss.
const IC_SLOTS: usize = 256;

/// One queued call to a sidecar's dedicated worker thread.
struct PyRequest {
    /// Call id — the promise it settles lives in `python_inflight_calls`.
    id: u64,
    /// Prebuilt wire request line (owned data; no arena references cross
    /// threads, so the worker can never touch the thread-local heap).
    line: String,
}

/// An in-flight python sidecar call: everything needed to (a) settle its
/// promise when the response arrives and (b) re-run it after a reload.
/// The burst check lives on the VM (not the child process): the child
/// cannot see the shared registry, and a running python function can only be
/// aborted by killing it — which the reload does. So each call records the
/// reload **burst** it was queued in; when its response arrives, a mismatch
/// means a NEW burst of reloads started after the call was queued — the
/// result came from old code (the reload already killed the child it ran
/// on). The pool is rebuilt from the current file and the call is re-run on
/// the fresh child, so the promise resolves with the current implementation
/// instead of settling stale or rejecting. Reloads folded into the same
/// burst leave the stamp alone, so a burst triggers exactly one re-run.
#[derive(Clone)]
struct InflightPyCall {
    promise: Value,
    /// Shared burst id at queue time — the stale check compares against it.
    burst: u64,
    /// Prebuilt wire request line, reused verbatim for the re-run.
    line: String,
}

/// A `.py` file's worker **pool**: child processes (each with its own worker
/// thread and request queue) so same-file calls run in parallel — a single
/// child is single-threaded, but K children give K-way concurrency. Children
/// spawn **lazily**: the first at import time, a new one only when every
/// existing child has an in-flight call (capped by `max`), so idle servers
/// pay for one python process, not K.
struct PythonWorker {
    /// One request queue per child; requests go to the least-busy child.
    senders: Vec<mpsc::Sender<PyRequest>>,
    /// In-flight request count per child (decremented by completions).
    busy: Vec<usize>,
    /// Round-robin tiebreak for equal-busy children (VM thread only).
    next: usize,
    /// Shared sidecar handles, one per child: the workers lock them per
    /// round-trip (responses can't be misattributed); the VM locks them at
    /// teardown to kill every child before joining.
    sidecars: Vec<Arc<Mutex<PythonSidecar>>>,
    /// Child pids, mirror of `sidecars` — teardown kills by pid first so a
    /// worker blocked in a read never holds up the join.
    pids: Vec<u32>,
    /// Top-level function names of the imported file (for the module object).
    funcs: Vec<String>,
    /// Worker threads, joined at VM teardown so every child is dead before
    /// the shared segment's backing file is unmapped/deleted.
    handles: Vec<std::thread::JoinHandle<()>>,
    /// Pool cap (`ALLOY_PYTHON_POOL`): growth stops here.
    max: usize,
    /// Shared-segment backing file + capacity, for spawning grown children.
    path: String,
    cap: usize,
    /// Raw base pointer of the shared segment (embed mode accesses it
    /// directly; the child mode only needs `path`).
    base: usize,
    /// The imported `.py` file, for spawning grown children.
    py_file: String,
    /// Completions channel clone for grown workers: `(src, child, id, resp)`.
    complete: mpsc::Sender<(String, usize, u64, String)>,
    /// Shared `.py` reload burst this pool's children were spawned in. A
    /// reload that starts a NEW burst on any thread bumps it; the next call
    /// on this VM compares and re-imports when it moved (kills these
    /// children, builds a fresh pool). Reloads folded into the same burst
    /// leave it alone — in-flight children survive, so a burst of reloads
    /// causes at most one rebuild.
    burst: u64,
    /// Per-pool per-call timeout override (from `Vm::set_python_timeout`);
    /// `None` falls back to `ALLOY_PYTHON_TIMEOUT_MS` / the built-in
    /// default. VM-scoped so one VM's short deadline never leaks into pools
    /// that other VMs (or other tests) spawn concurrently.
    timeout: Option<std::time::Duration>,
}

/// The isolated global scope of a loaded module: its values plus the
/// assigned-flags used for `typeof`/ReferenceError semantics inside it. Kept
/// here (not the stable name table) so module bindings can't collide with or
/// leak into the requirer's namespace.
struct ModuleGlobals {
    globals: Vec<Value>,
    defined: Vec<bool>,
}

impl PythonWorker {
    /// Queue `req` to the least-busy child, growing the pool when every child
    /// is busy (up to `max`). The caller must have incremented nothing yet.
    fn send(&mut self, req: PyRequest) -> bool {
        if self.senders.is_empty() {
            return false;
        }
        if self.least_busy().map_or(true, |i| self.busy[i] > 0) && self.senders.len() < self.max {
            self.grow();
        }
        let idx = match self.least_busy() {
            Some(i) => i,
            None => return false,
        };
        self.busy[idx] += 1;
        self.next += 1;
        self.senders[idx].send(req).is_ok()
    }

    /// Index of the child with the fewest in-flight calls (ties -> next).
    fn least_busy(&self) -> Option<usize> {
        if self.senders.is_empty() {
            return None;
        }
        let mut best = 0usize;
        let mut best_busy = usize::MAX;
        for (i, b) in self.busy.iter().enumerate() {
            if *b < best_busy {
                best = i;
                best_busy = *b;
            }
        }
        // Tie-break with the round-robin cursor when the top is a tie.
        let start = self.next % self.senders.len();
        if self.busy[start] == best_busy {
            Some(start)
        } else {
            Some(best)
        }
    }

    /// Spawn one more child (same file, same segment) and wire it into the
    /// pool. Returns false only if the child or its thread failed to start.
    fn grow(&mut self) -> bool {
        // One deadline for both backends: the child mode kills the process
        // at it; the embed mode requests a cooperative interrupt (pure-
        // python loops only — blocking C calls like time.sleep are not
        // interruptible in-process).
        let timeout = self.timeout.unwrap_or_else(|| {
            std::time::Duration::from_millis(
                std::env::var("ALLOY_PYTHON_TIMEOUT_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(PYTHON_CALL_TIMEOUT_MS),
            )
        });
        let sidecar = match PythonSidecar::start(&self.path, self.cap, self.base, &self.py_file, timeout) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("alloy python pool growth error: {}", e);
                return false;
            }
        };
        let idx = self.senders.len();
        let pid = sidecar.pid();
        let sidecar = Arc::new(Mutex::new(sidecar));
        let (tx, rx) = mpsc::channel::<PyRequest>();
        let worker_sidecar = sidecar.clone();
        let complete = self.complete.clone();
        let src = self.py_file.clone();
        let spawned = std::thread::Builder::new()
            .name("alloy-python-worker".to_string())
            .spawn(move || {
                while let Ok(req) = rx.recv() {
                    // One in-flight request per child: the mutex covers the
                    // whole round-trip so responses can't be misattributed
                    // (the child is single-threaded). The deadline kills a
                    // hung child so it can't park the queue forever.
                    let resp = {
                        let mut s = match worker_sidecar.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        s.call_line_timeout(&req.line, timeout)
                    };
                    let _ = complete.send((src.clone(), idx, req.id, resp));
                }
                // Queue closed (VM teardown): drop the sidecar, killing the
                // child.
                drop(worker_sidecar);
            });
        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                eprintln!("alloy python error: cannot start worker thread: {}", e);
                return false;
            }
        };
        self.senders.push(tx);
        self.busy.push(0);
        self.sidecars.push(sidecar);
        self.pids.push(pid);
        self.handles.push(handle);
        true
    }
}

/// One monomorphic inline-cache entry: a remembered (program, pc) site, the
/// object's shape pointer at that site, the resolved property offset, and the
/// property name's string identity (so a SetProperty site whose key changed
/// cannot write through a stale offset). Zero-cost to check: all fields are
/// plain compares.
#[derive(Clone, Copy)]
struct IcEntry {
    program: u32,
    pc: u32,
    /// `Rc::as_ptr(&shape)` of the object's shape at this site.
    shape: u64,
    offset: u32,
    /// `Value` word of the property name (for `Value::string` clones of the
    /// same `Rc`, the word is identical — identity comparison).
    prop: u64,
}

impl IcEntry {
    const EMPTY: IcEntry = IcEntry { program: u32::MAX, pc: u32::MAX, shape: 0, offset: 0, prop: 0 };
    #[inline(always)]
    fn matches(&self, program: u32, pc: u32, prop_bits: u64, shape: u64) -> bool {
        self.program == program && self.pc == pc && self.prop == prop_bits && self.shape == shape
    }
}

/// Two-way polymorphic slot: primary + secondary. Monomorphic sites hit primary
/// every time (one extra predictable branch vs before); 2-shape sites (e.g.
/// `{x}` vs `{x,y}` in one loop) hit secondary instead of thrashing a single
/// entry back and forth. 3+ shapes fall back to the slow map lookup (megamorphic).
#[derive(Clone, Copy)]
struct IcPoly { primary: IcEntry, secondary: IcEntry }
impl IcPoly {
    const EMPTY: IcPoly = IcPoly { primary: IcEntry::EMPTY, secondary: IcEntry::EMPTY };
}

/// Call-site cache: last callee seen at this `Call` pc + its function identity.
/// `dispatch_call` still validates `callee.bits()==cached`, so correctness is
/// unaffected; on hit we skip the `as_function` tag probe + Rc deref setup
/// checks via the leaf fast path below.
#[derive(Clone, Copy)]
struct CallIcEntry { callee_bits: u64, func_ptr: u64, params: u8 }
impl CallIcEntry { const EMPTY: CallIcEntry = CallIcEntry { callee_bits: 0, func_ptr: 0, params: 0 }; }



/// Translate one JS call argument onto the sidecar wire: a shared-segment
/// buffer (or a raw in-segment pointer number like `buf.ptr`) becomes a
/// segment offset (`p:`), which the Python side indexes into its mmap
/// zero-copy. Everything else passes as a number or string.
fn python_arg(v: &Value, base: usize, cap: usize) -> PyArg {
    if let Some((ptr, _len)) = v.as_buffer() {
        let p = ptr as usize;
        if p >= base && p < base + cap {
            return PyArg::Ptr((p - base) as u64);
        }
        return PyArg::Num(p as f64);
    }
    if let Some(n) = v.as_int() {
        let p = n as usize;
        if n >= 0 && p >= base && p < base + cap {
            return PyArg::Ptr((p - base) as u64);
        }
        return PyArg::Num(n as f64);
    }
    if let Some(n) = v.as_number() {
        let p = n as usize;
        if n >= 0.0 && p >= base && p < base + cap {
            return PyArg::Ptr((p - base) as u64);
        }
        return PyArg::Num(n);
    }
    if let Some(s) = v.as_str() {
        return PyArg::Str(s.to_string());
    }
    PyArg::Num(f64::NAN)
}

/// Expand spread positions in a value list (in source order): each position
/// whose mask bit is set holds an array whose elements are spliced in place.
fn expand_spreads(vals: Vec<Value>, mask: u16) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, v) in vals.into_iter().enumerate() {
        if mask & (1 << i) != 0 {
            if let Some(a) = v.as_array() {
                let a = a.borrow();
                out.extend(a.to_values());
            } else if let Some(s) = v.as_str() {
                // Strings are iterable in JS: spread by character.
                out.extend(s.chars().map(|c| Value::string(c.to_string())));
            } else if let Some(od) = v.as_object() {
                // Map/Set are iterable: a Map spreads its [k, v] entry pairs,
                // a Set its elements — both in insertion order.
                let c = od.borrow().container;
                if c == 1 || c == 2 {
                    for (k, val) in container_pairs(&v) {
                        if c == 1 {
                            out.push(Value::array(vec![k, val]));
                        } else {
                            out.push(val);
                        }
                    }
                } else {
                    out.push(v);
                }
            } else {
                out.push(v);
            }
        } else {
            out.push(v);
        }
    }
    out
}

/// Apply a fused arith code (0=Add 1=Sub 2=Mul 3=Div 4=Mod 5=BitAnd
/// 6=BitOr 7=BitXor 8=Shl 9=Shr 10=UShr 11=Pow). Mirrors the Add/Subtract/
/// Multiply/Divide/Modulo/BitAnd/BitOr/BitXor/Shl/Shr/UShr/Pow opcode
/// semantics exactly.
fn arith_apply(l: &Value, r: &Value, ar: u8) -> Value {
    match ar {
        0 => l.add(r),
        1 => l.subtract(r),
        2 => l.multiply(r),
        3 => l.divide(r),
        5 => l.bitand(r),
        6 => l.bitor(r),
        7 => l.bitxor(r),
        8 => l.shl(r),
        9 => l.shr(r),
        10 => l.ushr(r),
        11 => l.pow(r),
        4 => l.modulo(r),
        _ => Value::undefined(),
    }
}

/// The ArithChain i64 fast lane: `a ar b` when both are ints, with the same
/// edge cases as [`Value::add`]/[`subtract`]/[`multiply`]/[`divide`]/
/// [`modulo`] (overflow → f64, `0 * -5` → -0, `% 0` → NaN, `-9 % 3` → -0,
/// `/` always f64). Returns None for ar codes without an int fast lane
/// (bitwise/shift/pow) — the caller falls back to `arith_apply`, so the chain
/// is exactly equivalent to the sequence of plain opcodes.
#[inline(always)]
fn chain_arith_i64(a: i64, b: i64, ar: u8) -> Option<Value> {
    Some(match ar {
        0 => match a.checked_add(b) {
            Some(r) => Value::int(r),
            None => Value::number(a as f64 + b as f64),
        },
        1 => match a.checked_sub(b) {
            Some(r) => Value::int(r),
            None => Value::number(a as f64 - b as f64),
        },
        2 => match a.checked_mul(b) {
            Some(0) if (a < 0) != (b < 0) => Value::number(-0.0),
            Some(r) => Value::int(r),
            None => Value::number(a as f64 * b as f64),
        },
        3 => Value::number(a as f64 / b as f64),
        4 => {
            if b == 0 {
                Value::number(f64::NAN)
            } else if a % b == 0 && a < 0 {
                Value::number(-0.0)
            } else {
                Value::int(a % b)
            }
        }
        _ => return None,
    })
}

/// One register-ALU step: `l ar b` (b raw i64) with the int fast lane and
/// the generic Value fallback — used by the fixed-shape superinstructions.
#[inline(always)]
fn chain_step_i64(l: &Value, b: i64, ar: u8) -> Value {
    if let Some(a) = l.as_int() {
        if let Some(res) = chain_arith_i64(a, b, ar) {
            return res;
        }
    }
    arith_apply(l, &Value::int(b), ar)
}

// ---- per-slot SMI/number type feedback --------------------------------
//
// A parallel `kinds` table records, for every live stack slot, whether it
// currently holds a direct int (SMI), a direct f64 number, or anything else
// (cell, string, object, bool, …). The table is updated on every write that
// can change a slot's contents — push invalidates, store paths record the
// written kind — so KIND_INT / KIND_NUMBER are exact, never speculative: a
// slot marked INT is guaranteed to hold a direct int. The fused
// register-ALU, load/store, and compare handlers use it to skip the
// `as_int()` / `as_cell()` tag probes (and `Value::clone`'s payload match)
// on locals that are repeatedly int or number — e.g. collatz's `n` (an f64
// after `n / 2`) and `steps` (always an int). `ALLOY_NO_SMI_FB=1` disables
// feedback collection, which dead-ends every fast lane (kinds stay UNKNOWN)
// — the A/B switch used to measure the win.
const KIND_UNKNOWN: u8 = 0;
const KIND_INT: u8 = 1;
const KIND_NUMBER: u8 = 2;
const KIND_OTHER: u8 = 3;

/// The feedback kind a freshly-stored value should record. Cells are never
/// INT/NUMBER even though the value written *through* them might be: the
/// slot itself still holds the cell, and readers must deref it.
#[inline(always)]
fn kind_of_value(v: &Value) -> u8 {
    if v.is_int() {
        KIND_INT
    } else if v.is_number() {
        KIND_NUMBER
    } else {
        KIND_OTHER
    }
}

/// f64 fast lane for `a ar b` (b as f64): mirrors the (number, int)/(number,
/// number) branches of the Value ops exactly — no ToNumber, no string probe.
/// Returns None for ar codes without an f64 lane (bitwise/shift/pow coerce
/// via ToInt32 and must fall back to the generic Value op).
#[inline(always)]
fn f64_lane(a: f64, b: f64, ar: u8) -> Option<Value> {
    Some(match ar {
        0 => Value::number(a + b),
        1 => Value::number(a - b),
        2 => Value::number(a * b),
        3 => Value::number(a / b),
        4 => Value::number(a % b),
        _ => return None,
    })
}

/// Fast lane for `slots[slot] ar imm` when the slot's feedback kind is INT
/// or NUMBER: pure i64/f64 math, zero tag probes. `fast=false` means the
/// caller must use the generic Value path (unknown/other kind, or an ar code
/// with no lane for that kind).
#[inline(always)]
fn alu_local_imm(stack: &OperandStack, idx: usize, imm: i64, ar: u8) -> (Value, bool) {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                if let Some(res) = chain_arith_i64(Value::int_bits_raw(stack.at(idx).bits()), imm, ar)
                {
                    return (res, true);
                }
            }
            KIND_NUMBER => {
                if let Some(res) = f64_lane(f64::from_bits(stack.at(idx).bits()), imm as f64, ar) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Fast lane for `imm ar slots[slot]` (constant on the left — `3 * n`).
#[inline(always)]
fn alu_imm_local(stack: &OperandStack, idx: usize, imm: i64, ar: u8) -> (Value, bool) {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                if let Some(res) = chain_arith_i64(imm, Value::int_bits_raw(stack.at(idx).bits()), ar)
                {
                    return (res, true);
                }
            }
            KIND_NUMBER => {
                if let Some(res) = f64_lane(imm as f64, f64::from_bits(stack.at(idx).bits()), ar) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Fast lane for `slots[a] ar slots[b]` when both feedback kinds are known.
#[inline(always)]
fn alu_local_local(stack: &OperandStack, ia: usize, ib: usize, ar: u8) -> (Value, bool) {
    if ia < stack.len() && ib < stack.len() {
        match (stack.kind_of(ia), stack.kind_of(ib)) {
            (KIND_INT, KIND_INT) => {
                if let Some(res) = chain_arith_i64(
                    Value::int_bits_raw(stack.at(ia).bits()),
                    Value::int_bits_raw(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_NUMBER, KIND_INT) => {
                if let Some(res) = f64_lane(
                    f64::from_bits(stack.at(ia).bits()),
                    Value::int_bits_raw(stack.at(ib).bits()) as f64,
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_INT, KIND_NUMBER) => {
                if let Some(res) = f64_lane(
                    Value::int_bits_raw(stack.at(ia).bits()) as f64,
                    f64::from_bits(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_NUMBER, KIND_NUMBER) => {
                if let Some(res) = f64_lane(
                    f64::from_bits(stack.at(ia).bits()),
                    f64::from_bits(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Two-step `(slots[slot] ar1 imm1) ar2 imm2` entirely inside one lane when
/// the slot's kind is known — `seed = (seed * 48271) % 2147483648`. Both
/// steps stay in i64 for INT slots and f64 for NUMBER slots, matching the
/// generic chain exactly. None = fall back to the generic path.
#[inline(always)]
fn alu2_local_imm_imm(
    stack: &OperandStack,
    idx: usize,
    imm1: i64,
    ar1: u8,
    imm2: i64,
    ar2: u8,
) -> Option<Value> {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                let r1 = chain_arith_i64(Value::int_bits_raw(stack.at(idx).bits()), imm1, ar1)?;
                let r2 = chain_arith_i64(r1.as_int()?, imm2, ar2)?;
                return Some(r2);
            }
            KIND_NUMBER => {
                let r1 = f64_lane(f64::from_bits(stack.at(idx).bits()), imm1 as f64, ar1)?;
                let r2 = f64_lane(r1.as_number()?, imm2 as f64, ar2)?;
                return Some(r2);
            }
            _ => {}
        }
    }
    None
}

/// Two-step `(imm1 ar1 slots[slot]) ar2 imm2` — `n = 3 * n + 1` (constant
/// init on the left).
#[inline(always)]
fn alu2_imm_local_imm(
    stack: &OperandStack,
    idx: usize,
    imm1: i64,
    ar1: u8,
    imm2: i64,
    ar2: u8,
) -> Option<Value> {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                let r1 = chain_arith_i64(imm1, Value::int_bits_raw(stack.at(idx).bits()), ar1)?;
                let r2 = chain_arith_i64(r1.as_int()?, imm2, ar2)?;
                return Some(r2);
            }
            KIND_NUMBER => {
                let r1 = f64_lane(imm1 as f64, f64::from_bits(stack.at(idx).bits()), ar1)?;
                let r2 = f64_lane(r1.as_number()?, imm2 as f64, ar2)?;
                return Some(r2);
            }
            _ => {}
        }
    }
    None
}

/// Numeric comparison in the i64 lane — identical results to
/// `compare_values` for (int, int) operands (|v| ≤ 2^47 is exactly
/// representable as f64).
#[inline(always)]
fn cmp_i64(a: i64, b: i64, cmp: u8) -> bool {
    match cmp {
        0 => a < b,
        1 => a <= b,
        2 => a > b,
        3 => a >= b,
        4 => a == b,
        5 => a != b,
        6 => a == b,
        7 => a != b,
        _ => false,
    }
}

/// Map a raw compare OPCODE byte (Equal=16 … StrictNotEqual=90, as emitted
/// by the generic `Expr::Bin` path) to the semantic cmp code (0-7) that
/// `cmp_i64`/`compare_values` consume. The fused CmpLocal* opcodes already
/// carry the semantic code from the compiler; the generic-path fusions carry
/// the opcode ordinal and must translate.
#[inline(always)]
fn cmp_semantic(op: Opcode) -> u8 {
    match op {
        Opcode::Less => 0,
        Opcode::LessEqual => 1,
        Opcode::Greater => 2,
        Opcode::GreaterEqual => 3,
        Opcode::Equal => 4,
        Opcode::NotEqual => 5,
        Opcode::StrictEqual => 6,
        Opcode::StrictNotEqual => 7,
        _ => 6,
    }
}

/// Numeric comparison in the f64 lane — identical results to
/// `compare_values` for (number, int) operands (loose `==` and strict `===`
/// agree for two numbers).
#[inline(always)]
fn cmp_f64(a: f64, b: f64, cmp: u8) -> bool {
    match cmp {
        0 => a < b,
        1 => a <= b,
        2 => a > b,
        3 => a >= b,
        4 => a == b,
        5 => a != b,
        6 => a == b,
        7 => a != b,
        _ => false,
    }
}

fn strict_equal(l: &Value, r: &Value) -> bool {
    if let (Some(a), Some(b)) = (l.as_number(), r.as_number()) {
        a == b
    } else if let (Some(a), Some(b)) = (l.as_int(), r.as_int()) {
        a == b
    } else if let (Some(a), Some(b)) = (l.as_number(), r.as_int()) {
        a == b as f64
    } else if let (Some(a), Some(b)) = (l.as_int(), r.as_number()) {
        a as f64 == b
    } else if l.is_null() || l.is_undefined() {
        // Exact bit match: null === null / undefined === undefined only
        // (`null === undefined` is false even though `==` coerces them).
        l.bits() == r.bits()
    } else {
        l.same_type(r) && l.equal(r).is_truthy()
    }
}

/// Apply a fused compare code (0=< 1=<= 2=> 3=>= 4=== 5=!= 6=!== ...). Mirrors
/// the Less/Greater/LessEqual/GreaterEqual/Equal/NotEqual/StrictEqual
/// opcode semantics exactly (JS: string/string compares lexicographically,
/// everything else numerically; `!==` maps to loose NotEqual like the
/// compiler currently emits).
fn compare_values(l: &Value, r: &Value, cmp: u8) -> bool {
    // SMI fast path: int/int operands compare in the i64 lane — one tag check
    // per operand, no string probe, no ToNumber. Exact for every cmp code:
    // ints (|v| ≤ 2^47) are exactly representable as f64, so numeric ordering
    // and (in)equality agree with the JS ToNumber result. This is the hottest
    // path in loop-condition superinstructions like `i < 1000`.
    if let (Some(a), Some(b)) = (l.as_int(), r.as_int()) {
        return match cmp {
            0 => a < b,
            1 => a <= b,
            2 => a > b,
            3 => a >= b,
            4 => a == b,
            5 => a != b,
            6 => a == b,
            7 => a != b,
            _ => false,
        };
    }
    match cmp {
        0 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a < b
            } else {
                l.to_number() < r.to_number()
            }
        }
        1 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a <= b
            } else {
                l.to_number() <= r.to_number()
            }
        }
        2 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a > b
            } else {
                l.to_number() > r.to_number()
            }
        }
        3 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a >= b
            } else {
                l.to_number() >= r.to_number()
            }
        }
        4 => l.equal(&r).is_truthy(),
        5 => !l.equal(&r).is_truthy(),
        6 => strict_equal(l, r),
        7 => !strict_equal(l, r),
        _ => false,
    }
}

/// Operand stack and function locals, preallocated to a fixed size so the hot
/// push/pop paths are a raw store/load plus a pointer bump — no capacity
/// checks, no reallocation. `sp` is the stack pointer. Every slot always
/// holds a valid `Value` (pops/truncates clear to `Undefined`), so the array
/// can be dropped wholesale.
struct OperandStack {
    slots: Box<[Value]>,
    /// Parallel per-slot type feedback (KIND_* constants above): exact for
    /// every live slot — updated on every write that can change a slot's
    /// contents. Pops/truncates leave dead slots stale, which is safe because
    /// the next push that reuses the index invalidates the entry.
    kinds: Box<[u8]>,
    sp: usize,
    /// ALLOY_NO_SMI_FB=1 disables feedback collection: kinds stay UNKNOWN,
    /// so every fast lane dead-ends (the A/B switch for measuring the win).
    fb: bool,
}

impl OperandStack {
    fn new() -> Self {
        let mut v = Vec::with_capacity(STACK_SIZE);
        v.resize(STACK_SIZE, Value::undefined());
        OperandStack {
            slots: v.into_boxed_slice(),
            kinds: vec![KIND_UNKNOWN; STACK_SIZE].into_boxed_slice(),
            sp: 0,
            fb: std::env::var("ALLOY_NO_SMI_FB").is_err(),
        }
    }

    #[inline(always)]
    fn push(&mut self, val: Value) {
        debug_assert!(self.sp < STACK_SIZE, "operand stack overflow");
        unsafe { *self.slots.get_unchecked_mut(self.sp) = val; }
        // A pushed operand overwrites whatever a dead slot held — its kind
        // must not leak into a later fast lane (the truncate→push reuse
        // trap), so invalidate unconditionally.
        if self.fb {
            unsafe { *self.kinds.get_unchecked_mut(self.sp) = KIND_OTHER; }
        }
        self.sp += 1;
    }

    /// Feedback kind of a slot (valid index required, as with `at`).
    #[inline(always)]
    fn kind_of(&self, i: usize) -> u8 {
        unsafe { *self.kinds.get_unchecked(i) }
    }

    /// Record the kind of a slot after a store path wrote to it.
    #[inline(always)]
    fn mark_kind(&mut self, i: usize, k: u8) {
        if self.fb {
            unsafe { *self.kinds.get_unchecked_mut(i) = k; }
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> Value {
        // Underflow yields Undefined (matches the old Vec::pop().unwrap_or
        // behavior: the REPL's leftover-expression handling relies on it).
        if self.sp > 0 {
            self.sp -= 1;
            unsafe { std::mem::replace(self.slots.get_unchecked_mut(self.sp), Value::undefined()) }
        } else {
            Value::undefined()
        }
    }

    #[inline(always)]
    fn peek(&self) -> Value {
        if self.sp > 0 {
            self.slots[self.sp - 1].clone()
        } else {
            Value::undefined()
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.sp
    }

    /// Indexed read; callers must have established `i < sp` (slot addressing
    /// and the growth loops both guarantee this).
    #[inline]
    fn at(&self, i: usize) -> &Value {
        debug_assert!(i < self.sp);
        unsafe { self.slots.get_unchecked(i) }
    }

    /// Indexed write; see `at` for the invariant.
    #[inline]
    fn at_mut(&mut self, i: usize) -> &mut Value {
        debug_assert!(i < self.sp);
        unsafe { self.slots.get_unchecked_mut(i) }
    }

    /// Drop everything above `n` (function return, exception unwind). Slots
    /// are cleared so references are released and every slot stays valid.
    #[inline]
    fn truncate(&mut self, n: usize) {
        let n = n.min(self.sp);
        for s in &mut self.slots[n..self.sp] {
            *s = Value::undefined();
        }
        self.sp = n;
    }

    #[inline]
    fn clear(&mut self) {
        self.truncate(0);
    }

    /// Saved portion `[base..sp]`, used to suspend async invocations.
    fn save_from(&self, base: usize) -> Vec<Value> {
        self.slots[base..self.sp].to_vec()
    }

    /// Replace the whole stack with a saved continuation (never larger than
    /// `STACK_SIZE` — it was saved from this stack).
    fn restore(&mut self, values: Vec<Value>) {
        debug_assert!(values.len() <= STACK_SIZE);
        self.clear();
        let n = values.len();
        for (i, v) in values.into_iter().enumerate() {
            let k = kind_of_value(&v);
            self.slots[i] = v;
            self.mark_kind(i, k);
        }
        self.sp = n;
    }
}

pub struct Vm {
    stack: OperandStack,
    /// The current program's global view: aligned with
    /// `programs[program_id].globals` (functions reference globals by index
    /// into their own program's table).
    globals: Vec<Value>,
    /// Whether each slot in the current view was ever ASSIGNED (a `let` /
    /// `var` declaration or a store). Reading a global that was never
    /// assigned is a ReferenceError in JS — but `typeof` on it is
    /// "undefined" (the TypeOfGlobal opcode skips this check).
    global_defined: Vec<bool>,
    /// Stable name-keyed global table shared across all programs (REPL): a
    /// global set by one line is visible to every later program, even when
    /// their name indices differ.
    global_names: Vec<String>,
    stable_globals: Vec<Value>,
    /// Stable counterpart of `global_defined`, keyed like `stable_globals`.
    stable_defined: Vec<bool>,
    call_stack: Vec<CallFrame>,
    /// Closure cells of the currently executing functions (innermost last).
    cells_stack: Vec<Vec<Rc<RefCell<Value>>>>,
    /// Every program ever loaded (REPL: one per line). Function values carry a
    /// `program` id into this registry, so closures keep executing the bytecode
    /// of the program that defined them even after `set_program` swaps in a
    /// new one.
    programs: Vec<Program>,
    /// Id of the program currently being executed (index into `programs`).
    program_id: u32,
    /// Bytecode/constants of the currently executing program.
    bytecode: Vec<u8>,
    constants: Vec<Value>,
    /// Saved executions for suspended async invocations and `.then` callbacks.
    /// Promises reference these by id; ids move to `microtasks` when settled.
    continuations: HashMap<u64, Continuation>,
    next_cont_id: u64,
    /// Settled continuations waiting to run (FIFO). Records live in
    /// `microtask_arena`; the queue holds their addresses. The arena is
    /// bulk-reset (one cursor reset, no per-record free) whenever the queue
    /// drains — the GC-killer pattern.
    microtasks: VecDeque<usize>,
    microtask_arena: ChunkedArena,
    /// Pending `setTimeout` timers.
    timers: Vec<Timer>,
    /// Timestamp used to compute timer deadlines.
    epoch: Instant,
    /// Active exception handlers (innermost last), across all frames. Each
    /// records the frame that owns it so unwinding can scope correctly.
    handlers: Vec<Handler>,
    /// Set when a throw reaches the top level with no handler or async
    /// boundary; read (and cleared) by the CLI/tests via `take_error`.
    uncaught_exception: Option<Value>,
    /// Optional per-`run()` instruction cap (`None` = unlimited, the default
    /// so existing behavior is unchanged). A runaway script (`while(true){}`)
    /// stops the loop and records an uncaught error instead of hanging the
    /// host thread — the JS-side counterpart of the python sidecar's deadline.
    instruction_budget: Option<u64>,
    /// Set by `dispatch` when the budget ran out; consumed (and reset) by
    /// `run` so nested host-initiated dispatches aren't mislabeled uncaught.
    budget_exhausted: bool,
    /// When a native throws into an enclosing try/catch, `throw_value` has
    /// already unwound the stack and pushed the exception at the handler —
    /// this records the handler pc so the dispatcher can jump there instead
    /// of consuming the native's (meaningless) return value.
    native_throw_jump: Option<usize>,
    /// The receiver of the in-flight native call, for `VmHost::this_value`:
    /// the native branch of `dispatch_call` stashes the receiver here before
    /// invoking the native and restores the previous value afterwards, so
    /// re-entrant native calls (a native invoking a JS callback) see their
    /// own receiver. Method-call natives installed on Map/Set prototypes read
    /// their instance from this slot.
    native_this: Option<Value>,
    /// Local extent of the frame-less top-level scope, for the same purpose.
    top_locals_end: usize,
    shared: Arc<SidecarMemory>,
    /// The value heap: every runtime string/array/object allocates here and is
    /// bulk-freed when the VM drops (the GC-killer pillar). Installed as the
    /// active allocation context for the duration of `run()`.
    heap: ArenaHeap,
    /// Second-generation sweep trigger: run the major GC when the old
    /// generation has accumulated more than this many bytes of churn since
    /// the last sweep (churn = replaced globals/closures, whether they bump
    /// or reuse free space). Adaptive: backed off when a major reclaims
    /// little, tightened when it reclaims a lot.
    major_threshold: usize,
    /// Cumulative old-generation allocations at the last major GC; the delta
    /// from here is the churn that triggers the next one.
    last_major_alloc: usize,
    /// Incremental major-GC mark in progress. `Some` while a second-generation
    /// sweep is being prepared over multiple unit boundaries; each boundary
    /// records newly-reached boxes, traces `mark_budget` queued ones, and
    /// re-traces barrier-dirtied Rc structures — so no single request pays
    /// for walking the whole live graph.
    mark: Option<MarkState>,
    /// Worklist boxes traced per unit boundary while a mark is in progress
    /// (bounds the per-request mark stall).
    mark_budget: usize,

    /// Compiled regex programs, keyed by (pattern, flags). The compiled
    /// program is immutable and shared (Arc) across every literal evaluation;
    /// each MakeRegex builds a fresh per-object state (own lastIndex) from
    /// it. Compiling happens once per distinct literal, and the lexer already
    /// validated the pattern, so the runtime path is a cache hit.
    regex_cache: HashMap<(String, String), Arc<RegexCompiled>>,

    /// Cached Python module objects (one per imported `.py` file): the
    /// natives inside call into the matching sidecar below. Walked as GC
    /// roots so the object boxes survive arena sweeps.
    python_modules: HashMap<String, Value>,
    /// Dedicated worker threads for imported `.py` files (one per file), each
    /// servicing a request queue. Declared after `shared` so teardown order is
    /// explicit in `Drop` (children die and are joined before the segment's
    /// backing file is deleted).
    python_workers: HashMap<String, PythonWorker>,
    /// Per-call python timeout override for pools this VM spawns
    /// (`set_python_timeout`). `None` = read `ALLOY_PYTHON_TIMEOUT_MS` at
    /// pool growth, defaulting to `PYTHON_CALL_TIMEOUT_MS`.
    python_timeout: Option<std::time::Duration>,
    /// In-flight async python calls, by call id: the promise each will
    /// settle plus the src/gen/line needed to re-run a call that a reload
    /// aborted mid-flight. Completions arrive on `python_rx`; the VM thread
    /// resolves them (promises must settle on the VM thread — the values
    /// they carry allocate into the thread-local arena heap).
    python_inflight_calls: HashMap<u64, InflightPyCall>,
    /// Number of python calls still awaiting a completion (drives the event
    /// loop's poll cadence while they are in flight).
    python_inflight: usize,
    /// `.py` paths this VM has ever started a worker pool for (first import
    /// vs rebuild bookkeeping for `python_rebuilds`).
    python_started: std::collections::HashSet<String>,
    /// Number of times a `.py` worker pool was torn down and rebuilt after
    /// its first import (reload staleness). Asserted by the burst-coalescing
    /// test: a burst of reloads must rebuild exactly once, not once per
    /// reload. Read-only after run; not a GC root (plain strings).
    python_rebuilds: usize,
    /// Worker threads send `(src, child_idx, call_id, raw response line)`
    /// here; the VM drains it at event-loop boundaries, frees the child's
    /// in-flight slot, and resolves the matching promise.
    python_tx: mpsc::Sender<(String, usize, u64, String)>,
    python_rx: mpsc::Receiver<(String, usize, u64, String)>,
    /// Cross-thread `spawn(fn)`: the worker runs the function in an isolated
    /// VM and sends its serialized result here; the VM thread drains at
    /// event-loop boundaries and resolves the matching promise (values must
    /// settle on the VM thread — they allocate into its arena heap).
    spawn_tx: mpsc::Sender<(u64, Vec<u8>)>,
    spawn_rx: mpsc::Receiver<(u64, Vec<u8>)>,
    /// In-flight spawned tasks: the promise each will settle, by task id.
    spawn_inflight: HashMap<u64, Value>,
    /// Number of spawned tasks still awaiting a completion (keeps the event
    /// loop pumping while a worker runs).
    spawn_pending: usize,
    next_spawn_id: u64,
    /// Worker threads, joined at teardown so no task outlives the VM.
    spawn_workers: Vec<std::thread::JoinHandle<()>>,

    /// Loaded module programs: pid → its own global scope. Modules run in an
    /// isolated namespace (their `let`/`const`/`function` declarations never
    /// leak into the requirer, and vice versa) and keep their final globals
    /// here so functions the module exported can be called later — each call
    /// swaps this view in via `load_program`.
    modules: HashMap<u32, ModuleGlobals>,
    /// `require('./x.ajs')` → (exports object, shared generation cell, the
    /// generation it was loaded at), so each file runs exactly once per
    /// generation on this thread (module singletons, like Node). The cell is
    /// shared with every other VM, so a `reload()` on any thread invalidates
    /// this entry at the next require. Also walked as a GC root.
    require_cache: HashMap<String, (Value, Arc<ModuleGen>, u64)>,
    /// Stack of module paths currently being loaded, for circular-require
    /// detection (a loud error instead of Node's partial-module surprise).
    requiring: Vec<String>,
    /// Directory of the file currently executing (set by the CLI for the
    /// main script, pushed/popped around each required module). Relative
    /// `require('./x.ajs')` paths resolve against this, like Node; None
    /// means the process working directory (REPL).
    current_dir: Option<std::path::PathBuf>,
    /// Process-wide compiled-module cache shared with every spawn worker:
    /// compile once per module generation, load in any VM, and reload across
    /// threads without racing.
    registry: Arc<SharedModuleRegistry>,
    /// Cross-thread wake pipe: another VM's `send` routes a settlement to
    /// this VM's inbox (in `wake_tx`) and pushes a token here; the event
    /// loop's `recv_timeout` then returns immediately instead of waiting out
    /// its poll cadence. The handle (`wake_tx`) is stamped onto every
    /// promise this VM creates so settlements are routed back to the owner.
    wake_rx: std::sync::mpsc::Receiver<()>,
    wake_tx: Arc<WakeHandle>,
    /// Process-wide `.py` sidecar reload tracking shared with every spawn
    /// worker: a reload that starts a new burst on any thread re-imports the
    /// file on every VM's next call; reloads folded into the current burst
    /// don't (burst coalescing).
    py_registry: Arc<SharedPyRegistry>,
    /// Promises this VM has parked that are exposed to other threads (a
    /// channel `recv` waiter): their settlements can arrive via `wake_tx`,
    /// so the event loop keeps pumping until they settle. Also a GC root.
    cross_waiters: Vec<Value>,
    /// When set, `print` writes here instead of stdout (used by tests).
    output_sink: Option<Arc<Mutex<Vec<String>>>>,
    /// The seven error constructors seeded as one group (subclass prototypes
    /// chain to the base Error.prototype), built once per VM.
    error_seeds: Option<hashbrown::HashMap<String, Value>>,
    /// Direct-mapped 2-way polymorphic inline cache for GetProperty/SetProperty.
    ic: Box<[IcPoly; IC_SLOTS]>,
    /// Call-site cache for `Call`/`CallMethod` (direct-mapped like `ic`).
    call_ic: Box<[CallIcEntry; IC_SLOTS]>,
    /// Cached `ALLOY_OP_HIST` flag (checked once at construction, not per-instr).
    op_hist_on: bool,
    /// Backwards-jump trip counts for the baseline-JIT hypervisor: `pc -> trips`.
    /// Incremented only on taken backwards jumps (loop back-edges). When a count
    /// crosses `HOT_THRESHOLD`, a line is logged with `ALLOY_JIT_LOG=1`.
    backedge_counts: hashbrown::HashMap<usize, u32>,
}

/// A settled continuation ready to run. Records live in the microtask arena:
/// the bits are moved in on enqueue and moved out on resume, and the arena
/// slot is never dropped (bulk reset), which keeps the Rc accounting balanced
/// — the record owns exactly one reference and drops exactly one.
struct Microtask {
    id: u64,
    /// The fulfillment value or rejection reason.
    value: Value,
    /// True when the settlement was a rejection (await throws, .then skips
    /// its callback and rejects the chained promise).
    rejected: bool,
}

/// A saved execution: either a suspended async invocation (resumed with a
/// settled value) or a `.then` callback to invoke on settlement.
enum Continuation {
    Suspended {
        /// Operand stack of the async invocation, without the awaited value.
        stack: Vec<Value>,
        /// The async frame and everything it called, top last.
        frames: Vec<CallFrame>,
        cells: Vec<Vec<Rc<RefCell<Value>>>>,
        /// Active exception handlers owned by the saved frames.
        handlers: Vec<Handler>,
        /// Resume pc (after the `Await` opcode) in `program_id`.
        pc: usize,
        program_id: u32,
    },
    Callback {
        /// Optional `.then(onFulfilled)` handler (None = pass through).
        callback: Option<Value>,
        /// Optional `.then(_, onRejected)` handler (None = pass through).
        on_rejected: Option<Value>,
        /// Chained promise resolved with the callback's result.
        promise: Arc<Mutex<PromiseState>>,
    },
}

/// An active `try` block: where to unwind the stack to and where the handler
/// code lives.
#[derive(Clone)]
struct Handler {
    /// Stack depth at TryStart, in the owning frame's absolute stack.
    stack_depth: usize,
    /// Handler code entry (the thrown value is pushed at this pc).
    handler_pc: usize,
    /// Index of the frame that owns this handler (call_stack position).
    frame_depth: usize,
}

/// Result of routing a thrown value through the unwinder.
enum ThrowResult {
    /// Continue the dispatch loop at this pc (handler caught it, or the
    /// rejection was handed back to the caller as a promise).
    Jump(usize),
    /// The throw ended the current dispatch (resumed continuation boundary).
    EndDispatch,
    /// No handler anywhere: the VM recorded `uncaught_exception`.
    Abort,
}

struct Timer {
    /// Deadline in ms since the VM's `epoch`.
    when: f64,
    /// Insertion order, for FIFO firing of equal deadlines.
    seq: u64,
    /// Monotonic handle returned to JS (`setTimeout`/`setInterval` id),
    /// consumed by `clearTimeout`/`clearInterval`.
    id: u64,
    /// `Some(period)` for `setInterval` (rescheduled on fire); `None` for
    /// one-shot `setTimeout`.
    period: Option<f64>,
    callback: Value,
}

/// Per-path module generation counter, shared by every VM that loaded the
/// module. `reload()` bumps it and drops the compiled bytes, so each thread's
/// next `require` of that path — main VM, async server handler, or spawn
/// worker — sees the invalidation atomically, with no cross-thread cache to
/// lock and no shared Values (arena-heap pointers never leave their thread).
///
/// `.ajs` modules use only `gen` (one bump per reload; `require` recompiles
/// when it moved). `.py` sidecar modules use `burst` + `last_reload_ms`:
/// reloads arriving within [`PY_RELOAD_COALESCE_MS`] of the previous one are
/// the same **burst** and fold into it — only the first reload of a burst
/// bumps `burst`, so a rapid succession of reloads while calls are in flight
/// triggers exactly one pool rebuild / re-run instead of a cascade.
#[derive(Default)]
struct ModuleGen {
    /// `.ajs` reload generation: bumped once per reload, compared against the
    /// value each thread's cached copy was loaded at.
    gen: AtomicU64,
    /// `.py`: bumped when a NEW burst of reloads starts (the previous reload
    /// was more than the coalescing window ago). Pool and in-flight call
    /// stamps compare against this, so one burst = at most one abort.
    burst: AtomicU64,
    /// `.py`: wall-clock ms of the most recent reload, the burst-window
    /// anchor (`now - last_reload_ms > window` starts a new burst).
    last_reload_ms: AtomicU64,
}

/// Process-wide compiled-module cache shared by the main VM and every spawn
/// worker. Values cannot cross threads (the arena heap is thread-local), so
/// what is shared is the *compiled program bytes* — compile once, load in
/// any VM — plus a per-path generation for race-free `reload()`. Each thread
/// still runs a module's top-level code in its own isolated globals (Node's
/// worker model: per-worker module state); a reload invalidates every
/// thread's copy at its next require.
#[derive(Default)]
struct SharedModuleRegistry {
    /// canonical path -> (compiled program bytes, generation cell).
    compiled: Mutex<HashMap<String, (Arc<[u8]>, Arc<ModuleGen>)>>,
}

impl SharedModuleRegistry {
    /// Load-or-compile a module's program bytes exactly once per generation.
    /// The compile closure runs OUTSIDE the registry lock: holding it across a
    /// slow compile would serialize every module load process-wide (a slow
    /// compile blocking unrelated requires). The double-check on re-entry
    /// keeps compile-once semantics — a racing thread's duplicate compile is
    /// simply discarded in favor of the winner's cached bytes.
    fn get_or_compile(
        &self,
        canon: &str,
        compile: impl FnOnce() -> Result<Arc<[u8]>, String>,
    ) -> Result<(Arc<[u8]>, Arc<ModuleGen>), String> {
        {
            let map = self.compiled.lock().unwrap_or_else(|g| g.into_inner());
            if let Some((b, g)) = map.get(canon) {
                return Ok((b.clone(), g.clone()));
            }
        }
        let bytes = compile()?;
        let mut map = self.compiled.lock().unwrap_or_else(|g| g.into_inner());
        if let Some((b, g)) = map.get(canon) {
            return Ok((b.clone(), g.clone()));
        }
        let gen = Arc::new(ModuleGen::default());
        map.insert(canon.to_string(), (bytes.clone(), gen.clone()));
        // Bounded growth: a long-running server that requires thousands of
        // unique paths (or reloads through temp files) would otherwise grow
        // this map without limit. Flush the stale entries when far over cap
        // (the freshest insert stays); an evicted path's next require just
        // recompiles the same file — wasted work, never wrong bytes.
        if map.len() > MAX_COMPILED_MODULES {
            map.retain(|k, _| k == canon);
        }
        Ok((bytes, gen))
    }

    /// Invalidate a module across every thread: bump its generation cell (so
    /// threads holding a cached copy see the change on their next require)
    /// and drop the compiled bytes (so the next require on any thread
    /// recompiles from the current file). Returns whether it was cached.
    fn invalidate(&self, canon: &str) -> bool {
        let mut map = self.compiled.lock().unwrap_or_else(|g| g.into_inner());
        match map.remove(canon) {
            Some((_, gen)) => {
                gen.gen.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

/// How close two `.py` reloads must be to count as the same **burst**. Reloads
/// within this window of the previous one fold into it — they neither bump the
/// version nor tear down (kill) any pool — so a rapid burst of reloads while
/// calls are in flight causes exactly one pool rebuild / re-run instead of a
/// re-run cascade. A reload arriving after the window closes starts a new
/// Default `.py` reload coalescing window: reloads this close to the previous
/// one fold into the same burst. Override with `ALLOY_PYTHON_RELOAD_MS`
/// (widening it coalesces slower reload streams; narrowing it makes each
/// reload more likely to start a new burst and abort in-flight work).
const PY_RELOAD_COALESCE_MS: u64 = 250;

/// The `.py` reload coalescing window in milliseconds, read from
/// `ALLOY_PYTHON_RELOAD_MS` (default [`PY_RELOAD_COALESCE_MS`]). Read **per
/// call** rather than cached: reloads are rare (a process-env lookup here is
/// free) and it lets tests and operators change the window without a
/// restart. A value of 0 keeps the window semantics (only same-millisecond
/// reloads fold) — it does not disable folding, just narrows it.
fn py_reload_coalesce_ms() -> u64 {
    std::env::var("ALLOY_PYTHON_RELOAD_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(PY_RELOAD_COALESCE_MS)
}

/// Wall-clock milliseconds, the burst-window clock. Monotonicity isn't
/// required (a clock step only shifts where a burst boundary lands) but
/// saturation is: the first reload ever compares against 0, which is always
/// a new burst.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Process-wide reload tracking for `.py` sidecar modules, shared by the
/// main VM and every spawn worker. A `reload('./x.py')` on any thread
/// records the reload; each VM compares the shared burst against the one its
/// local pool was built at on every python call and re-imports (kills its
/// children, rebuilds the pool, re-reads the file) when a NEW burst started
/// — so a worker parked between calls serves the fresh child instead of the
/// stale one, while a burst of reloads folds into a single rebuild. No
/// values cross threads; only these counters do.
#[derive(Default)]
struct SharedPyRegistry {
    /// canonical .py path -> reload state cell.
    gen: Mutex<HashMap<String, Arc<ModuleGen>>>,
}

impl SharedPyRegistry {
    /// The reload-state cell for `canon`, creating it on first use (a VM's
    /// import records the current value; a reload folds into it). Bounded:
    /// past [`MAX_PY_TRACKED_MODULES`] unique paths, everything but the
    /// freshly-created cell is dropped — a later call on an evicted path
    /// recreates the cell (burst id restarts at 0, at worst one extra
    /// re-import after a reload, never stale bytes).
    fn cell(&self, canon: &str) -> Arc<ModuleGen> {
        let mut m = self.gen.lock().unwrap_or_else(|g| g.into_inner());
        let cell = m
            .entry(canon.to_string())
            .or_insert_with(|| Arc::new(ModuleGen::default()))
            .clone();
        if m.len() > MAX_PY_TRACKED_MODULES {
            m.retain(|k, _| k == canon);
        }
        cell
    }

    /// The current burst id for `canon`, creating the cell on first use.
    fn burst(&self, canon: &str) -> u64 {
        self.cell(canon).burst.load(Ordering::Relaxed)
    }

    /// Record a `reload('./x.py')` for `canon`. Returns `(had_cell,
    /// new_burst)`: whether the file was ever imported (a reload of something
    /// never imported is a no-op, like the .ajs path), and whether this
    /// reload STARTED a new burst. A new burst bumps the shared version so
    /// every VM re-imports at its next call and aborts in-flight calls;
    /// reloads within the coalescing window of the previous one fold into
    /// the current burst — they change the file's content the next rebuild
    /// reads, but don't tear down pools or abort work, which is what stops
    /// a burst of reloads from cascading into a burst of re-runs.
    fn invalidate(&self, canon: &str) -> (bool, bool) {
        // Read the window before taking the registry lock (a process-env
        // lookup must not run under the mutex).
        let window = py_reload_coalesce_ms();
        let m = self.gen.lock().unwrap_or_else(|g| g.into_inner());
        if let Some(g) = m.get(canon) {
            let now = now_ms();
            let last = g.last_reload_ms.swap(now, Ordering::Relaxed);
            let new_burst = now.saturating_sub(last) > window;
            if new_burst {
                g.burst.fetch_add(1, Ordering::Relaxed);
            }
            (true, new_burst)
        } else {
            (false, false)
        }
    }
}
#[derive(Clone)]
struct CallFrame {
    return_addr: usize,
    /// Program to resume in on return (index into `Vm::programs`).
    return_program: u32,
    base_slot: usize,
    /// Number of arguments actually passed (extra args beyond the params
    /// count are visible through `arguments`; missing params are not). The
    /// passed args occupy `[base_slot, base_slot + argc)`.
    argc: usize,
    /// Snapshot of the passed args for `arguments`, taken at call entry for
    /// functions whose body references `arguments` (the frame's local slots
    /// overwrite the arg region as the body runs, so a lazy read would see
    /// locals). `None` for functions that never use `arguments`.
    arg_values: Option<Vec<Value>>,
    /// The function value of the current frame (for LoadSelf recursion).
    fn_value: Value,
    /// cells_stack depth before this frame's cells were pushed.
    cells_len: usize,
    /// Local slot holding this invocation's promise, for async functions.
    promise_slot: Option<u8>,
    /// True when this frame was restored from a continuation: on return it
    /// must NOT jump back into the caller (which already ran when the async
    /// function suspended and returned its promise).
    resumed: bool,
    /// Whether this call's result is pushed back to the caller. False only
    /// for statement-position calls (CallKeep0/CallSpreadKeep0): the callee
    /// and args are consumed exactly as usual, but Return and the
    /// async-suspension path skip the result push.
    keep_result: bool,
    /// Length of the global handler stack when this frame was pushed, so
    /// popping the frame can discard its handlers.
    handlers_len: usize,
    /// One past the highest local slot written in this frame; exception
    /// unwinding must not truncate below this (locals survive into handlers).
    locals_end: usize,
    /// Operand-stack slot holding the receiver (`this`) of a method/new
    /// call — one slot below the first param (`base_slot - 1`). `None` for a
    /// plain call: `this` reads as undefined.
    this_slot: Option<usize>,
    /// True for a `new` invocation: on return, a non-object result is
    /// replaced by the instance at `this_slot` (JS constructor semantics).
    is_ctor: bool,
}

unsafe impl Send for Vm {}

impl Vm {
    pub fn new(program: Program) -> Self {
        Self::new_inner(
            program,
            None,
            Arc::new(SharedModuleRegistry::default()),
            Arc::new(SharedPyRegistry::default()),
            None,
        )
    }

    /// Override the per-call python timeout for pools THIS VM spawns
    /// (milliseconds; 0 disables). Defaults to `ALLOY_PYTHON_TIMEOUT_MS` or
    /// the built-in `PYTHON_CALL_TIMEOUT_MS`. VM-scoped, unlike the env var:
    /// a short deadline on one VM (a test, a per-tenant quota) never changes
    /// the process-global default that concurrently-spawned pools of other
    /// VMs inherit. Must be set before the first `.py` call (pools grow
    /// lazily during `run`).
    pub fn set_python_timeout(&mut self, ms: u64) {
        self.python_timeout = Some(std::time::Duration::from_millis(ms));
    }

    /// Create a VM whose `print` output is captured into the returned sink.
    pub fn with_output(program: Program) -> (Self, Arc<Mutex<Vec<String>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let vm = Self::new_inner(
            program,
            Some(sink.clone()),
            Arc::new(SharedModuleRegistry::default()),
            Arc::new(SharedPyRegistry::default()),
            None,
        );
        (vm, sink)
    }

    /// Construct a worker VM that shares the main thread's module registry
    /// (so `require` inside a spawned function reuses compiled bytes and
    /// honors cross-thread `reload()`) and python reload registry (so a
    /// `.py` reload that starts a new burst on any thread re-imports on the
    /// worker's next call), and
    /// resolves relative requires against the requirer's directory (Node
    /// semantics).
    fn new_worker(
        program: Program,
        registry: Arc<SharedModuleRegistry>,
        py_registry: Arc<SharedPyRegistry>,
        current_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self::new_inner(program, None, registry, py_registry, current_dir)
    }

    fn new_inner(
        program: Program,
        output: Option<Arc<Mutex<Vec<String>>>>,
        registry: Arc<SharedModuleRegistry>,
        py_registry: Arc<SharedPyRegistry>,
        current_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let (wake, wake_rx) = WakeHandle::new();
        // Safety net for hosts that leak VMs (a detached serve thread): reap
        // python sidecar children whose parent process died, BEFORE the
        // segment sweep below — a killed orphan releases its segment-file
        // handle, so the sweep can delete the leaked file (Windows cannot
        // delete an open file). Rate-limited; live children of live VMs are
        // never touched.
        crate::python_sidecar::reap_orphaned_python_children();
        let shared = Arc::new(SidecarMemory::new(shared_memory_capacity()));
        // Program 0's globals seed the stable table; the initial view matches
        // the stable table 1:1.
        let mut global_names = Vec::with_capacity(program.globals.len());
        let mut stable_globals = Vec::with_capacity(program.globals.len());
        let mut stable_defined = Vec::with_capacity(program.globals.len());
        // Error constructors are seeded as ONE group (subclass prototypes
        // chain to the base Error.prototype), built once per VM so
        // `e instanceof Error` resolves to the same object everywhere.
        let error_group: Option<hashbrown::HashMap<String, Value>> =
            if program.globals.iter().any(|g| is_error_name(g)) {
                Some(error_ctor_map())
            } else {
                None
            };
        for name in &program.globals {
            global_names.push(name.clone());
            let v = match &error_group {
                Some(g) if is_error_name(name) => {
                    g.get(name).cloned().unwrap_or(Value::undefined())
                }
                _ => seed_global(name, output.clone(), shared.clone()),
            };
            stable_globals.push(v.clone());
            // Builtins (natives, NaN, Infinity, Math, …) are readable; an
            // unknown name seeds to undefined and is NOT defined.
            stable_defined.push(!v.is_undefined());
        }
        let globals = stable_globals.clone();
        let global_defined = stable_defined.clone();
        let mut programs = Vec::with_capacity(1);
        programs.push(program);
        let bytecode = programs[0].bytecode.clone();
        let constants = programs[0].constants.clone();
        let (python_tx, python_rx) = mpsc::channel();
        let (spawn_tx, spawn_rx) = mpsc::channel();
        Self {
            stack: OperandStack::new(),
            globals,
            global_defined,
            global_names,
            stable_globals,
            stable_defined,
            call_stack: Vec::with_capacity(64),
            cells_stack: Vec::new(),
            programs,
            program_id: 0,
            bytecode,
            constants,
            continuations: HashMap::new(),
            next_cont_id: 0,
            microtasks: VecDeque::new(),
            microtask_arena: ChunkedArena::new(1 << 16),
            timers: Vec::new(),
            epoch: Instant::now(),
            handlers: Vec::new(),
            uncaught_exception: None,
        instruction_budget: match std::env::var("ALLOY_VM_BUDGET").ok().and_then(|s| s.parse::<u64>().ok()) { Some(0) => None, Some(n) => Some(n), None => Some(50_000_000) },
        budget_exhausted: false,
            native_throw_jump: None,
            // The receiver of the in-flight native call (method natives read
            // their instance from `this_value`). Restored around re-entrant
            // calls inside the native branch of `dispatch_call`.
            native_this: None,
            top_locals_end: 0,
            modules: HashMap::new(),
            require_cache: HashMap::new(),
            requiring: Vec::new(),
            current_dir,
            registry,
            py_registry,
            wake_rx,
            wake_tx: Arc::new(wake),
            cross_waiters: Vec::new(),
            shared,
            heap: ArenaHeap::new(1 << 16),
            major_threshold: MAJOR_THRESHOLD_INIT,
            last_major_alloc: 0,
            mark: None,
            mark_budget: 256,
            regex_cache: HashMap::new(),
            python_modules: HashMap::new(),
            python_workers: HashMap::new(),
            python_timeout: None,
            python_inflight_calls: HashMap::new(),
            python_inflight: 0,
            python_started: std::collections::HashSet::new(),
            python_rebuilds: 0,
            python_tx,
            python_rx,
            spawn_tx,
            spawn_rx,
            spawn_inflight: HashMap::new(),
            spawn_pending: 0,
            next_spawn_id: 0,
            spawn_workers: Vec::new(),
            output_sink: output,
            error_seeds: error_group,
            ic: Box::new([IcPoly::EMPTY; IC_SLOTS]),
            call_ic: Box::new([CallIcEntry::EMPTY; IC_SLOTS]),
            op_hist_on: std::env::var("ALLOY_OP_HIST").is_ok(),
            backedge_counts: hashbrown::HashMap::new(),
        }
    }

    /// Track that a local slot is live, so exception unwinding preserves it.
    fn record_local(&mut self, idx: usize) {
        match self.call_stack.last_mut() {
            Some(f) => f.locals_end = f.locals_end.max(idx + 1),
            None => self.top_locals_end = self.top_locals_end.max(idx + 1),
        }
    }

    /// The value of the last uncaught exception, if any (cleared by the read).
    pub fn take_error(&mut self) -> Option<Value> {
        self.uncaught_exception.take()
    }

    /// Make `pid` the currently executing program (REPL lines and closures that
    /// outlive their defining program), and rebuild its global view from the
    /// stable table so bytecode from any program finds its globals by index.
    fn load_program(&mut self, pid: u32) {
        // Stash the current view if it belongs to a module, so module-global
        // mutations made by the code that just ran are preserved (the
        // authoritative Vec lives in `modules`, and the per-call view is
        // moved in/out of it). Non-module views are rebuilt from the stable
        // name table below, so they need no stash.
        if let Some(entry) = self.modules.get_mut(&self.program_id) {
            entry.globals = std::mem::take(&mut self.globals);
            entry.defined = std::mem::take(&mut self.global_defined);
        }
        self.bytecode = self.programs[pid as usize].bytecode.clone();
        self.constants = self.programs[pid as usize].constants.clone();
        self.program_id = pid;
        // A module program installs its own isolated globals directly.
        if let Some(entry) = self.modules.get_mut(&pid) {
            self.globals = std::mem::take(&mut entry.globals);
            self.global_defined = std::mem::take(&mut entry.defined);
            return;
        }
        let names = self.programs[pid as usize].globals.clone();
        let mut globals = Vec::with_capacity(names.len());
        let mut defined = Vec::with_capacity(names.len());
        for name in &names {
            match self.global_names.iter().position(|n| n == name) {
                Some(idx) => {
                    globals.push(self.stable_globals[idx].clone());
                    defined.push(self.stable_defined[idx]);
                }
                None => {
                    let v = self.seed_global_named(name);
                    self.global_names.push(name.clone());
                    self.stable_globals.push(v.clone());
                    self.stable_defined.push(!v.is_undefined());
                    globals.push(v);
                    defined.push(self.stable_defined[self.stable_defined.len() - 1]);
                }
            }
        }
        self.globals = globals;
        self.global_defined = defined;
    }

    /// Seed a single global by name. Error constructors come from the
    /// per-VM cached group (built once so subclass prototypes chain to one
    /// shared Error.prototype and `e instanceof Error` holds everywhere —
    /// constructor seeding, module loads, and REPL swaps all route here).
    fn seed_global_named(&mut self, name: &str) -> Value {
        if is_error_name(name) {
            let group = self.error_seeds.get_or_insert_with(error_ctor_map);
            return group.get(name).cloned().unwrap_or(Value::undefined());
        }
        seed_global(name, self.output_sink.clone(), self.shared.clone())
    }

    pub fn with_shared_memory(program: Program, shared: Arc<SidecarMemory>) -> Self {
        let mut vm = Self::new(program);
        vm.shared = shared;
        vm
    }

    /// Swap in a new program (REPL use) while keeping globals, shared memory
    /// and the output sink intact. Values are preserved by name through the
    /// stable table, so a later line referencing `x` sees earlier values
    /// regardless of how the per-program name indices differ.
    pub fn set_program(&mut self, program: Program) {
        let pid = self.programs.len() as u32;
        self.programs.push(program);
        self.load_program(pid);
    }

    /// Set the directory of the script being executed, so relative
    /// `require('./x.ajs')` paths resolve against it like Node (the CLI calls
    /// this with the main script's path).
    pub fn set_script_path(&mut self, path: &str) {
        self.current_dir = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_path_buf());
    }

    /// Node-style `require` resolution. A bare specifier (`require('pkg')`)
    /// walks `node_modules/` from the requiring file's directory upward;
    /// `./`-relative and absolute paths resolve from the file's directory.
    /// Either way the candidate is tried as: the exact file, then `.ajs`,
    /// then `.ax`, then (for a directory) `package.json` `main`, then
    /// `index.ajs` / `index.ax`.
    /// Whether a specifier explicitly targets a python sidecar module
    /// (ends in `.py`). Only these route to the python machinery — a `.ajs`
    /// or extension-less specifier is always a JS module, so `require` and
    /// `reload` never spawn a python child for a javascript file.
    fn is_py_specifier(path: &str) -> bool {
        path.trim_end().to_ascii_lowercase().ends_with(".py")
    }

    /// Resolve a `.py` sidecar specifier like `require` does — relative to
    /// the requiring file's directory, with the `.py` extension supplied if
    /// omitted — returning the canonical path when the file exists. Used by
    /// `reload('./x.py')` and `require('./x.py')` (workers load python
    /// modules this way) and to canonicalize import keys.
    fn resolve_py_path(&self, path: &str) -> Option<String> {
        let p = std::path::Path::new(path);
        let base = self
            .current_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        let abs = if abs.extension().is_none() {
            abs.with_extension("py")
        } else {
            abs
        };
        if !abs.is_file() {
            return None;
        }
        std::fs::canonicalize(&abs)
            .ok()
            .map(|c| c.to_string_lossy().to_string())
    }

    fn resolve_module_path(&self, path: &str) -> Result<String, String> {
        let p = std::path::Path::new(path);
        let base = self
            .current_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let is_bare = !path.starts_with('.') && !p.is_absolute();
        if is_bare {
            // Walk ancestors, trying `<dir>/node_modules/<path>` at each.
            let mut dir = Some(base.clone());
            while let Some(d) = dir {
                if let Some(found) = self.resolve_file_or_dir(&d.join("node_modules").join(path)) {
                    return Ok(found);
                }
                dir = d.parent().map(|pd| pd.to_path_buf());
            }
            return Err(path.to_string());
        }
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        self.resolve_file_or_dir(&abs)
            .ok_or_else(|| path.to_string())
    }

    /// Try a candidate in Node's LOAD_AS_FILE / LOAD_AS_DIRECTORY order:
    /// exact file, `.ajs`, `.ax`, then (directory) `package.json` `main`,
    /// `index.ajs`, `index.ax`. Returns a canonical path.
    fn resolve_file_or_dir(&self, base: &std::path::Path) -> Option<String> {
        let canon = |p: &std::path::Path| {
            std::fs::canonicalize(p)
                .map(|c| c.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.to_string_lossy().to_string())
        };
        if base.is_file() {
            return Some(canon(base));
        }
        for ext in ["ajs", "ax"] {
            let p = base.with_extension(ext);
            if p.is_file() {
                return Some(canon(&p));
            }
        }
        if base.is_dir() {
            // package.json `main` first, then index files (Node order).
            if let Some(main) = Self::read_package_main(&base.join("package.json")) {
                let mp = base.join(&main);
                if let Some(found) = self.resolve_file_or_dir(&mp) {
                    return Some(found);
                }
            }
            for name in ["index.ajs", "index.ax"] {
                let p = base.join(name);
                if p.is_file() {
                    return Some(canon(&p));
                }
            }
        }
        None
    }

    /// Extract the `"main"` field from a package.json — a minimal scan (no
    /// full JSON parse), sufficient for the common `"main": "src/x.js"`
    /// shape.
    fn read_package_main(pj: &std::path::Path) -> Option<String> {
        let s = std::fs::read_to_string(pj).ok()?;
        let idx = s.find("\"main\"")?;
        let rest = &s[idx + 6..];
        let colon = rest.find(':')?;
        let after = rest[colon + 1..].trim_start();
        let q = after.strip_prefix('"')?;
        let end = q.find('"')?;
        Some(q[..end].to_string())
    }

    /// `require('./x.ajs')`: load the file (once — cached by canonical path),
    /// run its top level in an isolated module scope, and return its exports
    /// object. Missing files, syntax errors, and circular requires throw
    /// loudly (`require()` itself throws, like Node). Exported functions keep
    /// working afterwards: each call swaps the module's own globals view in
    /// via `load_program`.
    /// `reload('./x.ajs')`: drop the cached module so the NEXT `require` of
    /// that file re-runs it — hot reload for long-running servers (Node's
    /// `delete require.cache[resolve(path)]`). Old references keep working:
    /// the previous program, its isolated globals, and the cells behind old
    /// exports/imports stay valid, so functions already obtained from the
    /// module still run against their own state. Builtins and unresolvable
    /// paths return false (nothing to reload), like `delete` on a cache miss.
    ///
    /// Note: reloading accumulates the old module's state (it must stay alive
    /// for existing references), so hot-reloading in a tight loop grows
    /// memory — fine for dev edits, not for per-request invalidation.
    pub fn reload_module(&mut self, path: &str) -> bool {
        if self.require_builtin(path).is_some() {
            return false;
        }
        // `.py` sidecar module: record the reload in the shared registry so
        // every VM (this one, spawn workers, async handlers) re-imports the
        // file at its next python call. Only a reload that STARTS a new burst
        // (outside the coalescing window) also drops THIS VM's pool + cached
        // module object now, killing its children — the abort that makes
        // in-flight calls re-run. Reloads folded into the current burst leave
        // the pool alone (its in-flight children survive), which is what
        // stops a burst of reloads from cascading into a burst of re-runs.
        // Only explicit `.py` specifiers route here — a JS require/reload
        // must never spawn a python child (or the python cache) for a .ajs
        // file.
        if Self::is_py_specifier(path) {
            if let Some(canon) = self.resolve_py_path(path) {
                let (had_shared, new_burst) = self.py_registry.invalidate(&canon);
                let had_local = self.python_workers.contains_key(&canon);
                if new_burst {
                    if had_local {
                        self.shutdown_python_worker(&canon);
                    }
                    self.python_modules.remove(&canon);
                }
                return had_shared || had_local;
            }
            return false;
        }
        let canon = match self.resolve_module_path(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let had_local = self.require_cache.remove(&canon).is_some();
        // Drop the shared compiled bytes AND bump the generation cell, so
        // every VM that holds a cached copy — this thread, async handlers,
        // or any spawn worker — recompiles and re-runs the file at its next
        // require, with no cross-thread cache to lock.
        let had_shared = self.registry.invalidate(&canon);
        had_local || had_shared
    }

    /// `require('fs')` / `require('alloy:fs')` return the builtin module
    /// object (Node's builtin modules), cached so it's a singleton.
    fn require_builtin(&mut self, path: &str) -> Option<Value> {
        const BUILTINS: &[&str] = &[
            "fs", "http", "memory", "channel", "Promise", "Date", "Math", "JSON",
            "Number", "Object", "Array", "String", "console", "setTimeout",
            "setInterval", "clearTimeout", "clearInterval", "queueMicrotask",
            "parseInt", "parseFloat", "isNaN", "Error", "TypeError", "RangeError",
            "ReferenceError", "SyntaxError", "EvalError", "URIError",
        ];
        let name = path.strip_prefix("alloy:").unwrap_or(path);
        if !BUILTINS.contains(&name) {
            return None;
        }
        let key = format!("builtin:{}", name);
        if let Some((v, _, _)) = self.require_cache.get(&key) {
            return Some(v.clone());
        }
        let v = self.seed_global_named(name);
        if v.is_undefined() {
            return None;
        }
        // Builtins are per-VM natives (no cross-thread state), so they use a
        // private generation cell that `reload` never bumps (reload returns
        // false for builtins without touching it).
        self.require_cache
            .insert(key, (v.clone(), Arc::new(ModuleGen::default()), 0));
        Some(v)
    }

    pub fn require_module(&mut self, path: &str) -> Value {
        // Node-style builtins win over node_modules lookups.
        if let Some(b) = self.require_builtin(path) {
            return b;
        }
        // `.py` sidecar module: `require('./x.py')` returns the python module
        // object (one native per top-level function) — the way a spawn
        // worker, which never runs the importing program's top level, loads a
        // python file. Reload-aware through the shared burst registry. Only
        // explicit `.py` specifiers route here — `require('./math.ajs')`
        // must stay on the JS module path.
        if Self::is_py_specifier(path) {
            if let Some(canon) = self.resolve_py_path(path) {
                return self.python_module(&canon);
            }
        }
        let canon = match self.resolve_module_path(path) {
            Ok(c) => c,
            Err(_) => {
                self.throw_exception(Value::string(format!(
                    "Error: Cannot find module '{}'",
                    path
                )));
                return Value::undefined();
            }
        };
        // Fast path: locally cached AND the module hasn't been reloaded on
        // any thread since. The generation cell is shared with every other
        // VM, so a stale copy (reload happened elsewhere) falls through and
        // reloads below.
        if let Some((exports, gen, gen_at)) = self.require_cache.get(&canon) {
            if gen.gen.load(Ordering::Relaxed) == *gen_at {
                return exports.clone();
            }
            self.require_cache.remove(&canon);
        }
        if self.requiring.iter().any(|p| *p == canon) {
            self.throw_exception(Value::string(format!(
                "Error: Circular require of '{}'",
                path
            )));
            return Value::undefined();
        }
        // Compiled program bytes come from the process-wide registry: the
        // first thread to require a path reads/compiles it exactly once
        // (under the registry lock — no double compile, no racing), and
        // every later thread on any VM reuses the bytes.
        let (bytes, gen) = match self
            .registry
            .get_or_compile(&canon, || Self::load_module_bytes(&canon, path))
        {
            Ok(ok) => ok,
            Err(msg) => {
                self.throw_exception(Value::string(msg));
                return Value::undefined();
            }
        };
        let program = match Program::from_bytes(&bytes) {
            Ok(p) => p,
            Err(e) => {
                self.throw_exception(Value::string(format!(
                    "SyntaxError: failed to load '{}': {}",
                    path, e
                )));
                return Value::undefined();
            }
        };
        let exports_pairs = program.exports.clone();
        let pid = self.programs.len() as u32;
        self.programs.push(program);
        // Fresh module scope: builtins only — nothing from the requirer's
        // globals leaks in (and vice versa). `require` itself is seeded so
        // modules can require other modules.
        let names = self.programs[pid as usize].globals.clone();
        let mut globals = Vec::with_capacity(names.len());
        let mut defined = Vec::with_capacity(names.len());
        for name in &names {
            let v = self.seed_global_named(name);
            defined.push(!v.is_undefined());
            globals.push(v);
        }
        // Live bindings: every exported slot becomes a cell so the exports
        // object and `import { x }` bindings alias the module's own storage
        // (ESM live-binding semantics — later mutations are visible). The
        // module's own LoadGlobal/StoreGlobal and every external read go
        // through the cell.
        for (_, binding) in &exports_pairs {
            if let Some(i) = names.iter().position(|n| n == binding) {
                let v = std::mem::replace(&mut globals[i], Value::undefined());
                globals[i] = Value::cell(v);
            }
        }
        self.modules.insert(pid, ModuleGlobals { globals, defined });
        self.requiring.push(canon.clone());
        let saved_dir = self.current_dir.take();
        // Nested requires inside this module resolve relative to the module's
        // own directory (Node semantics).
        self.current_dir = std::path::Path::new(&canon)
            .parent()
            .map(|p| p.to_path_buf());

        // ---- Save the caller's execution state (the module runs as a
        // nested synchronous dispatch, like a host-initiated call). ----
        let base_slot = self.stack.len();
        if base_slot + FRAME_BUDGET > STACK_SIZE {
            self.requiring.pop();
            self.current_dir = saved_dir;
            self.throw_exception(Value::string("RangeError: require stack overflow".to_string()));
            return Value::undefined();
        }
        let saved_stack = self.stack.save_from(0);
        let saved_program = self.program_id;
        let saved_cells = self.cells_stack.len();
        // The module runs with NO caller handlers active: an uncaught module
        // throw must not jump into the caller's bytecode mid-module. We
        // re-route it into the caller's handlers after restoring state.
        let saved_handlers = std::mem::take(&mut self.handlers);
        let saved_top_locals_end = self.top_locals_end;
        let saved_native_jump = self.native_throw_jump.take();
        let saved_uncaught = self.uncaught_exception.take();
        self.call_stack.push(CallFrame {
            return_addr: 0,
            return_program: saved_program,
            base_slot,
            argc: 0,
            arg_values: None,
            fn_value: Value::undefined(),
            cells_len: saved_cells,
            promise_slot: None,
            resumed: false,
            keep_result: false,
            handlers_len: 0,
            locals_end: base_slot,
            this_slot: None,
            is_ctor: false,
        });
        self.load_program(pid);
        self.dispatch(0);
        self.call_stack.pop();
        self.stack.truncate(base_slot);

        // Collect exports from the module's view (still installed). Each
        // pair is (public name, binding): an alias `export { a as b }` reads
        // global `a` but publishes it as `b`; `export default` publishes the
        // reserved `\0default` binding under "default".
        let mut map = HashMap::new();
        for (name, binding) in &exports_pairs {
            let idx = self.programs[pid as usize]
                .globals
                .iter()
                .position(|g| g == binding);
            let v = match idx {
                Some(i) => self.globals.get(i).cloned().unwrap_or(Value::undefined()),
                None => Value::undefined(),
            };
            map.insert(name.clone(), v);
        }
        let exports = Value::object(map);
        // Restore the caller. `load_program` stashes the module view back
        // into `modules` (program_id is still the module's) and rebuilds the
        // caller's view from the stable table.
        self.load_program(saved_program);
        self.cells_stack.truncate(saved_cells);
        self.handlers = saved_handlers;
        self.top_locals_end = saved_top_locals_end;
        self.stack.restore(saved_stack);
        self.native_throw_jump = saved_native_jump;
        self.requiring.pop();
        self.current_dir = saved_dir;
        let module_exc = self.uncaught_exception.take();
        self.uncaught_exception = saved_uncaught;

        // A throw the module didn't catch propagates to the caller's
        // handlers: `require()` throws. Failed modules are NOT cached, so a
        // later require retries (Node semantics).
        if let Some(exc) = module_exc {
            match self.throw_value(exc) {
                ThrowResult::Jump(p) => self.native_throw_jump = Some(p),
                ThrowResult::EndDispatch | ThrowResult::Abort => {}
            }
            return Value::undefined();
        }
        let gen_at = gen.gen.load(Ordering::Relaxed);
        self.require_cache
            .insert(canon, (exports.clone(), gen, gen_at));
        exports
    }

    /// Read + compile a module file once, returning program bytes for the
    /// shared registry. `.ajs` source compiles to bytecode; `.ax` files are
    /// precompiled and validated (must be a module). Later threads reuse the
    /// bytes via `Program::from_bytes` instead of recompiling.
    fn load_module_bytes(canon: &str, requested: &str) -> Result<Arc<[u8]>, String> {
        let bytes = std::fs::read(canon).map_err(|e| {
            format!("Error: Cannot find module '{}' ({})", requested, e)
        })?;
        if canon.ends_with(".ax") {
            match Program::from_bytes(&bytes) {
                Ok(p) if p.is_module => Ok(Arc::from(bytes)),
                Ok(_) => Err(format!(
                    "Error: '{}' is not a module — precompile it with --module",
                    requested
                )),
                Err(e) => Err(format!(
                    "SyntaxError: failed to load '{}': {}",
                    requested, e
                )),
            }
        } else {
            let src = String::from_utf8(bytes).map_err(|_| {
                format!("SyntaxError: '{}' is not valid UTF-8 source", requested)
            })?;
            let p = Compiler::compile_module(&src).map_err(|e| {
                format!("SyntaxError: failed to compile '{}': {}", requested, e)
            })?;
            p.to_bytes()
                .map(Arc::from)
                .map_err(|e| format!("internal error: cannot serialize '{}': {}", requested, e))
        }
    }

    /// A pending promise stamped with this VM's wake handle, as the raw Arc
    /// (used by `then`'s chained promise, which is both stored in the
    /// continuation and returned as a Value).
    fn new_promise_arc(&self) -> Arc<Mutex<PromiseState>> {
        Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Pending,
            continuations: Vec::new(),
            owner: Some(self.wake_tx.clone()),
        }))
    }

    #[inline(always)]
    fn push(&mut self, val: Value) {
        self.stack.push(val);
    }

    #[inline(always)]
    fn pop(&mut self) -> Value {
        self.stack.pop()
    }

    #[inline(always)]
    fn peek(&self) -> Value {
        self.stack.peek()
    }

    /// Cap the number of bytecode instructions `run()` may execute. `None`
    /// (the default) = unlimited; existing behavior is unchanged.
    pub fn set_instruction_budget(&mut self, budget: Option<u64>) {
        self.instruction_budget = budget;
    }

    pub fn run(&mut self) -> Value {
        // All values created during this run allocate into the VM's heap.
        let heap_ptr: *mut ArenaHeap = &mut self.heap;
        let _g = HeapGuard::set(heap_ptr);
        self.budget_exhausted = false;
        let mut result = self.dispatch(0);
        if self.budget_exhausted {
            self.uncaught_exception =
                Some(Value::string("Error: instruction budget exhausted".to_string()));
            return result;
        }
        if self.uncaught_exception.is_some() {
            // An uncaught top-level throw terminates the program.
            return result;
        }
        // After the script (or timer callback) finishes, run settled promise
        // continuations and due timers until nothing is left. This is what
        // drives suspended async functions to completion.
        self.drive_event_loop();
        // TEMP profiling: dump the opcode histogram once per process.
        if let Some(h) = op_hist() {
            if let Ok(mut g) = h.lock() {
                eprintln!("=== op histogram ===");
                let mut v: Vec<(u8, u64)> = g
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c > 0)
                    .map(|(i, c)| (i as u8, *c))
                    .collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                for (i, c) in v {
                    let name = Opcode::from_u8(i)
                        .map(|o| o.name().to_string())
                        .unwrap_or_else(|| "?".into());
                    eprintln!("{:>12}  {}", c, name);
                }
                *g = [0u64; 256];
            }
        }
        // Unit boundary: promote everything still live (globals, closures,
        // channels, the result) into the old generation and reclaim the rest.
        self.promote_and_reclaim(Some(&mut result));
        result
    }

    /// Walk every persistent root: young boxes are promoted, old boxes are
    /// recorded into the incremental mark when one is in progress, and Rc
    /// structures are traversed. `map`/`visited` are the per-unit dedup
    /// structures; `result` is the run's return value (also a root).
    fn walk_roots(
        &mut self,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
        result: Option<&mut Value>,
    ) {
        // Live operand stack (the server reclaims mid-script, so the script's
        // own live values are roots here).
        for i in 0..self.stack.sp {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut self.stack.slots[i]);
        }
        for g in &mut self.globals {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
        }
        for g in &mut self.stable_globals {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
        }
        // Cached Python module objects (their natives hold no heap values,
        // but the object boxes themselves are arena-allocated).
        for m in self.python_modules.values_mut() {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), m);
        }
        // `require`d module exports and the isolated global scopes behind
        // them: both stay live for the VM's lifetime (module singletons), so
        // the arena must not reclaim their boxes between requires.
        for (e, _, _) in self.require_cache.values_mut() {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
        }
        for m in self.modules.values_mut() {
            for g in m.globals.iter_mut() {
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
            }
        }
        for cs in &mut self.cells_stack {
            for c in cs.iter() {
                walk_cell(&mut self.heap, map, visited, mark.as_deref_mut(), c);
            }
        }
        // Parked cross-thread channel waiters: promises whose settlement may
        // be in flight on another thread; walking them keeps their status
        // values (and, via the promise's continuations, nothing heap — ids
        // only) alive until the routed delivery lands.
        for w in &mut self.cross_waiters {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), w);
        }
        for t in &mut self.timers {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut t.callback);
        }
        for cont in self.continuations.values_mut() {
            match cont {
                Continuation::Suspended { stack, cells, .. } => {
                    for v in stack.iter_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), v);
                    }
                    for cs in cells {
                        for c in cs {
                            walk_cell(&mut self.heap, map, visited, mark.as_deref_mut(), c);
                        }
                    }
                }
                Continuation::Callback { callback, on_rejected, promise } => {
                    if let Some(cb) = callback.as_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), cb);
                    }
                    if let Some(cb) = on_rejected.as_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), cb);
                    }
                    let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
                    if let PromiseStatus::Fulfilled(val) | PromiseStatus::Rejected(val) =
                        &mut ps.status
                    {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), val);
                    }
                }
            }
        }
        // Microtask records (normally empty at a boundary; defensive).
        let mts: Vec<usize> = self.microtasks.iter().copied().collect();
        for addr in mts {
            let mut mt = unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut mt.value);
            unsafe {
                std::ptr::write(addr as *mut Microtask, mt);
            }
        }
        if let Some(r) = result {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), r);
        }
    }

    /// Escape-analysis pass at a unit boundary (script run, HTTP request).
    ///
    /// 1. Promote walk: copy live young values into the old generation.
    /// 2. Dirty-box scan: the write barrier flagged every old box written
    ///    since the last boundary; re-trace those (promoting any young values
    ///    they now hold) and clear the flags — this replaces the old O(live)
    ///    old-box recursion with O(dirty) work.
    /// 3. Incremental mark slice: when a second-generation sweep is being
    ///    prepared, trace `mark_budget` queued boxes plus everything the
    ///    barrier dirtied (cells/promises/channels) — so a huge live graph
    ///    is spread across many requests instead of stalling one.
    /// 4. Record promoted boxes into the mark (their interiors were already
    ///    fully traced by the promote walk).
    /// 5. Sweep the young generation.
    /// 6. When the mark drains, sweep the old generation (non-copying) and
    ///    adapt the trigger to the free-space ratio; otherwise it continues
    ///    at the next boundary.
    fn promote_and_reclaim(&mut self, result: Option<&mut Value>) {
        let mut mark = self.mark.take();
        // Start a second-generation sweep when the old gen has churned past
        // the threshold since the last one.
        if mark.is_none()
            && self.heap.old_alloc_total().saturating_sub(self.last_major_alloc)
                >= self.major_threshold
        {
            mark = Some(MarkState::new());
        }
        let mut result_opt = result;
        let mut map = PromoteMap::new();
        let mut visited = std::collections::HashSet::new();
        // 1. Promote walk (young → old; old boxes recorded into the mark).
        self.walk_roots(&mut map, &mut visited, mark.as_mut(), result_opt.as_deref_mut());
        // 2. Dirty-box scan (young values written into old boxes + marking).
        self.scan_dirty_old_boxes(&mut map, &mut visited, mark.as_mut());
        // 3. Incremental mark slice: budgeted worklist + barrier-dirtied Rc
        //    structures.
        if let Some(m) = &mut mark {
            let batch: Vec<usize> = {
                let n = self.mark_budget.min(m.worklist.len());
                m.worklist.split_off(m.worklist.len() - n)
            };
            for addr in batch {
                self.trace_box(addr, &mut map, &mut visited, Some(&mut *m));
            }
            let dirty = std::mem::take(&mut m.dirty_rc);
            let mut v2 = std::collections::HashSet::new();
            for d in dirty {
                self.trace_rc_dirty(d, &mut map, &mut v2, Some(&mut *m));
            }
        }
        // 4. Record promoted boxes into the mark (interiors already traced).
        if let Some(m) = &mut mark {
            for &addr in map.values() {
                m.insert_box(&self.heap, addr);
            }
        }
        // 5. Sweep the young generation.
        sweep_young(&mut self.heap, &map);
        // 6. Finish the sweep when the mark drains.
        if let Some(m) = mark {
            if m.is_done() {
                let free_after = {
                    sweep_old_mark_sweep(&mut self.heap, &m.set);
                    self.heap.free_bytes()
                };
                self.last_major_alloc = self.heap.old_alloc_total();
                // Adapt the trigger: a sweep that found little garbage means
                // the old gen is mostly live — back off; one that found a lot
                // means churn is heavy — get more aggressive. The clamps only
                // ever shrink growth toward MIN / stall shrink at MIN — never
                // the reverse — so a manually-tuned threshold outside
                // [MIN, MAX] keeps its range.
                let used = self.heap.used_old();
                let t = self.major_threshold;
                if free_after * 4 < used {
                    self.major_threshold = (t * 2).min(MAJOR_THRESHOLD_MAX.max(t));
                } else if free_after * 2 > used {
                    self.major_threshold = (t / 2).max(MAJOR_THRESHOLD_MIN.min(t));
                }
                self.mark = None;
            } else {
                self.mark = Some(m);
            }
        }
    }

    /// Re-trace every old box the write barrier dirtied since the last unit
    /// boundary: promote young values written into it and, while a sweep is
    /// being prepared, record the box into the mark. The flags were cleared
    /// by the collector.
    fn scan_dirty_old_boxes(
        &mut self,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        let dirty = self.heap.collect_dirty_old_boxes();
        for (addr, kind) in dirty {
            match kind as u64 {
                KIND_ARRAY => {
                    let inner = unsafe { &*(addr as *const RefCell<ArrayData>) };
                    match &mut *inner.borrow_mut() {
                        // Packed ints hold no heap references.
                        ArrayData::Ints(_) => {}
                        ArrayData::Values(v) => {
                            for e in v.iter_mut() {
                                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                            }
                        }
                    }
                }
                KIND_OBJECT => {
                    let inner = unsafe { &*(addr as *const RefCell<ObjectData>) };
                    let mut od = inner.borrow_mut();
                    for e in od.values.iter_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                    }
                    // Map/Set entries hold heap values too (table rebuilt
                    // against remapped key addresses; order remapped in
                    // place).
                    if let Some(cd) = od.entries.as_mut() {
                        walk_container_entries(cd, &mut self.heap, map, visited, mark.as_deref_mut());
                    }
                }
                _ => {}
            }
        }
    }

    /// Trace one old box's interior for the incremental mark: mark every box
    /// it references and promote any young values (defensive — the dirty-box
    /// scan normally handles those first).
    fn trace_box(
        &mut self,
        addr: usize,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        let kind = self.heap.kind_of(addr);
        match kind as u64 {
            KIND_ARRAY => {
                let inner = unsafe { &*(addr as *const RefCell<ArrayData>) };
                match &mut *inner.borrow_mut() {
                    // Packed ints hold no heap references.
                    ArrayData::Ints(_) => {}
                    ArrayData::Values(v) => {
                        for e in v.iter_mut() {
                            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                        }
                    }
                }
            }
            KIND_OBJECT => {
                let inner = unsafe { &*(addr as *const RefCell<ObjectData>) };
                let mut od = inner.borrow_mut();
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut od.proto);
                for e in od.values.iter_mut() {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                }
                // Map/Set entries: keys and values are heap values the mark
                // must keep alive (table rebuilt against remapped addresses,
                // order remapped in place).
                if let Some(cd) = od.entries.as_mut() {
                    walk_container_entries(cd, &mut self.heap, map, visited, mark.as_deref_mut());
                }
            }
            _ => {}
        }
    }

    /// Re-trace an Rc-backed structure the write barrier dirtied (a closure
    /// cell, promise, or channel): it may now reference unmarked values.
    /// Uses its own `visited` so cycles can re-enter.
    fn trace_rc_dirty(
        &mut self,
        d: RcDirtyRef,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        match d {
            RcDirtyRef::Cell(cell) => {
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut cell.borrow_mut());
            }
            RcDirtyRef::Promise(p) => {
                let mut ps = p.lock().unwrap_or_else(|g| g.into_inner());
                if let PromiseStatus::Fulfilled(val) | PromiseStatus::Rejected(val) = &mut ps.status
                {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), val);
                }
            }
            RcDirtyRef::Channel(st) => {
                let mut g = st.lock().unwrap_or_else(|g| g.into_inner());
                for m in g.queue.iter_mut() {
                    if let ChannelItem::Raw(v) = m {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), v);
                    }
                }
                for w in g.waiters.iter_mut() {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), w);
                }
            }
        }
    }

    fn drive_event_loop(&mut self) {
        loop {
            // Settle completed python sidecar calls, spawn workers, and
            // cross-thread channel sends first: resolving their promises
            // enqueues the microtasks the awaits are parked on.
            self.drain_python_completions();
            self.drain_spawn_completions();
            self.drain_cross_thread_inbox();
            // Run settled continuations until the microtask queue is empty,
            // then reclaim every record with a single arena reset (no
            // per-record free).
            while let Some(addr) = self.microtasks.pop_front() {
                let mt =
                    unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
                self.resume(mt);
            }
            self.microtask_arena.reset();
            // Drop waiters that settled this iteration; the event loop must
            // keep pumping only while one is still pending.
            self.retain_cross_waiters();
            // Fire any timers whose deadline has passed.
            let now = self.epoch.elapsed().as_secs_f64() * 1000.0;
            let due: Vec<(u64, Value, Option<f64>)> = self
                .timers
                .iter()
                .filter(|t| t.when <= now)
                .map(|t| (t.id, t.callback.clone(), t.period))
                .collect();
            if !due.is_empty() {
                self.timers.retain(|t| t.when > now);
                for (id, cb, period) in due {
                    // `setInterval`: reschedule the next occurrence before
                    // firing (so a slow callback drifts rather than queues
                    // back-to-back runs). The rescheduled timer keeps the
                    // SAME id, so `clearInterval` still cancels it.
                    if let Some(p) = period {
                        let when =
                            self.epoch.elapsed().as_secs_f64() * 1000.0 + p.max(0.0);
                        let seq = self.next_cont_id;
                        self.next_cont_id += 1;
                        self.timers.push(Timer {
                            when,
                            seq,
                            id,
                            period: Some(p),
                            callback: cb.clone(),
                        });
                        self.timers.sort_by(|a, b| {
                            a.when
                                .partial_cmp(&b.when)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then(a.seq.cmp(&b.seq))
                        });
                    }
                    // Isolate the callback from stale outer handlers; report
                    // anything it throws uncaught.
                    let saved_handlers = std::mem::take(&mut self.handlers);
                    self.call_value(&cb, &[]);
                    self.handlers = saved_handlers;
                    if let Some(err) = self.uncaught_exception.take() {
                        eprintln!("uncaught exception in timer callback: {}", err);
                    }
                }
                continue;
            }
            // Nothing to run right now: wait for the next timer, a python
            // completion, a spawn worker, or a cross-thread channel send, or
            // finish. While calls/workers/waiters are in flight the loop
            // polls at millisecond cadence so a slow task never blocks
            // timers or other continuations; a parked waiter additionally
            // waits on the wake pipe so a send from another thread resumes it
            // immediately instead of on the poll cadence.
            let pending_work = self.python_inflight > 0
                || self.spawn_pending > 0
                || !self.cross_waiters.is_empty();
            if self.timers.is_empty() && !pending_work {
                break;
            }
            let next = self
                .timers
                .iter()
                .map(|t| t.when)
                .fold(f64::INFINITY, f64::min);
            let wait = ((next - now).max(0.0)).min(if pending_work { 2.0 } else { 1000.0 }) as u64;
            if wait > 0 {
                if !self.cross_waiters.is_empty() {
                    // Interruptible: a routed settlement pushes a wake token.
                    let _ = self
                        .wake_rx
                        .recv_timeout(std::time::Duration::from_millis(wait));
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
            }
        }
        // Drop any execution state left behind by continuations so the next
        // program (REPL) starts from a clean stack.
        self.stack.clear();
        self.call_stack.clear();
        self.cells_stack.clear();
        self.handlers.clear();
    }

    /// Drain channel settlements routed here from other threads: decode each
    /// byte payload into this VM's heap (the values are arena pointers valid
    /// only here) and resolve the waiter locally, enqueuing its continuations
    /// as microtasks. Runs on the owning VM thread, like the python/spawn
    /// drains.
    fn drain_cross_thread_inbox(&mut self) {
        for (promise, bytes) in self.wake_tx.take_deliveries() {
            let mut pos = 0;
            let value = decode_spawn_value(&bytes, &mut pos);
            self.resolve_promise(&promise, value);
        }
    }

    /// Drop waiters that have settled (their microtasks are already queued),
    /// keeping only the still-pending ones the event loop must keep pumping
    /// for.
    fn retain_cross_waiters(&mut self) {
        self.cross_waiters.retain(|p| {
            p.as_promise()
                .map(|pr| {
                    matches!(
                        pr.lock().unwrap_or_else(|g| g.into_inner()).status,
                        PromiseStatus::Pending
                    )
                })
                .unwrap_or(false)
        });
    }

    /// One non-blocking pump: settle completed python sidecar calls and run
    /// whatever microtasks they enqueued (plus any queued before). Used by
    /// both the blocking `drive_pending` and the concurrent server's accept
    /// loop, which must interleave pumping with accepting new connections.
    fn pump_async_once(&mut self) {
        self.drain_python_completions();
        self.drain_spawn_completions();
        self.drain_cross_thread_inbox();
        while let Some(addr) = self.microtasks.pop_front() {
            let mt = unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
            self.resume(mt);
        }
        self.microtask_arena.reset();
        self.retain_cross_waiters();
    }

    /// Pump pending async work until quiet: settle python sidecar calls and
    /// run the microtasks they enqueue. The HTTP server calls this after a
    /// handler suspends on `await python.f(...)` so the handler can resume
    /// and call `res.send` before the response is written.
    fn drive_pending_inner(&mut self) {
        loop {
            self.pump_async_once();
            if self.python_inflight == 0
                && self.spawn_pending == 0
                && self.microtasks.is_empty()
                && self.cross_waiters.is_empty()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Run a settled continuation: either restore a suspended async invocation
    /// (throwing on rejection) or invoke a `.then` callback (skipped on
    /// rejection, which instead rejects the chained promise).
    fn resume(&mut self, mt: Microtask) {
        let Microtask { id, value, rejected } = mt;
        match self.continuations.remove(&id) {
            Some(Continuation::Suspended { stack, frames, cells, handlers, pc, program_id }) => {
                self.stack.restore(stack);
                self.call_stack = frames;
                self.cells_stack = cells;
                self.handlers = handlers;
                self.load_program(program_id);
                if rejected {
                    // The awaited promise was rejected: throw in the resumed
                    // context so try/catch inside the async function sees it.
                    match self.throw_value(value) {
                        ThrowResult::Jump(p) => {
                            self.dispatch(p);
                        }
                        ThrowResult::EndDispatch => {}
                        ThrowResult::Abort => {}
                    }
                } else {
                    self.push(value);
                    self.dispatch(pc);
                }
            }
            Some(Continuation::Callback { callback, on_rejected, promise }) => {
                let handler = if rejected { on_rejected } else { callback };
                let handler = match handler {
                    Some(h) => h,
                    None => {
                        // No handler for this settlement: propagate rejection
                        // (or resolve fulfilled) down the chain.
                        if rejected {
                            self.reject_promise(&promise, value);
                        } else {
                            self.resolve_promise(&promise, value);
                        }
                        return;
                    }
                };
                // The callback runs in isolation: handlers from the enclosing
                // (suspended) program must not catch its throws.
                let saved_handlers = std::mem::take(&mut self.handlers);
                let result = self.call_value(&handler, &[value]);
                self.handlers = saved_handlers;
                if let Some(err) = self.uncaught_exception.take() {
                    self.reject_promise(&promise, err);
                } else {
                    self.resolve_promise(&promise, result);
                }
            }
            None => {}
        }
    }

    /// Route a thrown value: innermost handler wins; otherwise the nearest
    /// async boundary converts it to a rejection; otherwise it is uncaught.
    fn throw_value(&mut self, exc: Value) -> ThrowResult {
        // 1. Innermost active handler, if any.
        if let Some(h) = self.handlers.last().cloned() {
            // An async boundary between the throw site and this handler must
            // reject first: an async function's error never propagates
            // synchronously to an enclosing caller's try.
            if let Some(bi) = ((h.frame_depth + 1)..self.call_stack.len())
                .rev()
                .find(|&i| self.call_stack[i].promise_slot.is_some())
            {
                return self.reject_at_boundary(bi, exc);
            }
            // Unwind frames above the handler's frame.
            while self.call_stack.len() > h.frame_depth {
                let f = self.call_stack.pop().unwrap();
                self.stack.truncate(f.base_slot);
                self.cells_stack.truncate(f.cells_len);
                self.handlers.truncate(f.handlers_len);
            }
            // Keep the handler frame's locals (they were written before the
            // throw and the catch body may read them); drop operand garbage.
            let floor = match self.call_stack.last() {
                Some(f) => f.locals_end,
                None => self.top_locals_end,
            };
            self.stack.truncate(h.stack_depth.max(floor));
            self.push(exc);
            self.handlers.pop();
            return ThrowResult::Jump(h.handler_pc);
        }
        // 2. No handler: the nearest async boundary rejects its promise.
        if let Some(bi) = (0..self.call_stack.len())
            .rev()
            .find(|&i| self.call_stack[i].promise_slot.is_some())
        {
            return self.reject_at_boundary(bi, exc);
        }
        // 3. Uncaught at the top level.
        self.uncaught_exception = Some(exc);
        ThrowResult::Abort
    }

    /// Reject the async function at frame `bi`, hand its promise back to the
    /// boundary's caller, and stop unwinding there.
    fn reject_at_boundary(&mut self, bi: usize, exc: Value) -> ThrowResult {
        let b = self.call_stack[bi].clone();
        let promise = self.stack.at(b.base_slot + b.promise_slot.unwrap() as usize).clone();
        if let Some(p) = promise.as_promise() {
            self.reject_promise(p, exc);
        }
        while self.call_stack.len() > bi {
            let f = self.call_stack.pop().unwrap();
            self.stack.truncate(f.base_slot);
            self.cells_stack.truncate(f.cells_len);
            self.handlers.truncate(f.handlers_len);
        }
        self.stack.truncate(b.base_slot);
        self.push(promise);
        if b.resumed {
            // The caller already received the promise; end this dispatch.
            return ThrowResult::EndDispatch;
        }
        ThrowResult::Jump(b.return_addr)
    }

    /// Settle `promise` with `value`. If `value` is itself a promise, this
    /// invocation's continuations are transferred to it (JS flattening) and
    /// rejections propagate as rejections.
    fn resolve_promise(&mut self, promise: &Arc<Mutex<PromiseState>>, value: Value) {
        if let Some(inner) = value.as_promise() {
            let inner_status = inner.lock().unwrap_or_else(|g| g.into_inner()).status.clone();
            match inner_status {
                PromiseStatus::Fulfilled(v) => {
                    return self.resolve_promise(promise, v);
                }
                PromiseStatus::Rejected(v) => {
                    return self.reject_promise(promise, v);
                }
                PromiseStatus::Pending => {
                    let conts = {
                        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
                        std::mem::take(&mut ps.continuations)
                    };
                    inner
                        .lock()
                        .unwrap_or_else(|g| g.into_inner())
                        .continuations
                        .extend(conts);
                    return;
                }
            }
        }
        self.note_rc_dirty(RcDirtyRef::Promise(promise.clone()));
        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
        ps.status = PromiseStatus::Fulfilled(value.clone());
        let conts = std::mem::take(&mut ps.continuations);
        drop(ps);
        for id in conts {
            self.enqueue_microtask(id, value.clone(), false);
        }
    }

    /// Bump-allocate a settled-continuation record in the microtask arena and
    /// enqueue its address. The arena slot stays valid until the next drain.
    fn enqueue_microtask(&mut self, id: u64, value: Value, rejected: bool) {
        let ptr = self.microtask_arena.alloc_at(Microtask { id, value, rejected });
        self.microtasks.push_back(ptr as usize);
    }

    /// Total bytes the microtask arena has handed out since the last drain
    /// (used by tests to prove the bulk-reset reclaims it).
    #[cfg(test)]
    fn microtask_arena_used(&self) -> usize {
        self.microtask_arena.used()
    }

    /// Young-generation bytes currently in use (tests assert it returns to
    /// ~0 after each unit boundary).
    #[cfg(test)]
    fn heap_used_young(&self) -> usize {
        self.heap.used_young()
    }

    fn reject_promise(&mut self, promise: &Arc<Mutex<PromiseState>>, value: Value) {
        self.note_rc_dirty(RcDirtyRef::Promise(promise.clone()));
        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
        ps.status = PromiseStatus::Rejected(value.clone());
        let conts = std::mem::take(&mut ps.continuations);
        drop(ps);
        for id in conts {
            self.enqueue_microtask(id, value.clone(), true);
        }
    }

    /// Register `.then(onFulfilled, onRejected)` callbacks on `promise` and
    /// return the chained promise that settles with the callback's result.
    fn then(
        &mut self,
        promise: &Value,
        callback: Value,
        on_rejected: Option<Value>,
    ) -> Value {
        let src = match promise.as_promise() {
            Some(p) => p.clone(),
            None => return Value::undefined(),
        };
        // Non-function handlers are skipped (the settlement passes through).
        let is_fn = |v: &Value| v.is_function() || v.is_native();
        let callback = if is_fn(&callback) { Some(callback) } else { None };
        let on_rejected = on_rejected.filter(&is_fn);
        let chained = self.new_promise_arc();
        let id = self.next_cont_id;
        self.next_cont_id += 1;
        self.continuations.insert(
            id,
            Continuation::Callback { callback, on_rejected, promise: chained.clone() },
        );
        let mut flush: Option<(Value, bool)> = None;
        {
            let mut st = src.lock().unwrap_or_else(|g| g.into_inner());
            match &st.status {
                PromiseStatus::Pending => st.continuations.push(id),
                PromiseStatus::Fulfilled(v) => flush = Some((v.clone(), false)),
                PromiseStatus::Rejected(v) => flush = Some((v.clone(), true)),
            }
        }
        if let Some((v, rejected)) = flush {
            self.enqueue_microtask(id, v, rejected);
        }
        Value::promise(chained)
    }

    /// Register a one-shot (`period: None`) or repeating (`period: Some`)
    /// timer and return its handle id. Repeating timers are rescheduled with
    /// the same period each time they fire.
    fn schedule_timer(&mut self, callback: Value, ms: f64, period: Option<f64>) -> u64 {
        let when = self.epoch.elapsed().as_secs_f64() * 1000.0 + ms.max(0.0);
        let seq = self.next_cont_id;
        let id = self.next_cont_id;
        self.next_cont_id += 1;
        self.timers.push(Timer { when, seq, id, period, callback });
        self.timers.sort_by(|a, b| a.when.partial_cmp(&b.when).unwrap_or(std::cmp::Ordering::Equal).then(a.seq.cmp(&b.seq)));
        id
    }

    /// Cancel a pending timer by its handle id (returned by `setTimeout` /
    /// `setInterval`). Idempotent: unknown ids are ignored.
    fn clear_timer(&mut self, id: u64) {
        self.timers.retain(|t| t.id != id);
    }

    /// `spawn(fn, ...args)`: run `fn` on a dedicated worker thread with an
    /// isolated VM (its own arena heap, program registry, and globals),
    /// returning a promise the VM thread settles when the worker reports the
    /// result. `fn` and `args` cross the thread boundary as serialized bytes
    /// — the isolated message-passing model, never references.
    /// Functions/natives/channels/promises inside the value graph coerce to
    /// `undefined` (data crosses, code and state do not). Named channels
    /// (`channel.create`/`get`) and the shared module registry are the
    /// cross-VM communication paths, so a worker can park on
    /// `await ch.recv()` and be woken by a send from another thread.
    fn vm_spawn_fn(&mut self, f: &Value, args: &[Value]) -> Value {
        let promise = self.new_promise();
        let reject = |vm: &mut Self, why: &str| {
            if let Some(pr) = promise.as_promise() {
                vm.reject_promise(pr, Value::string(why.to_string()));
            }
        };
        let Some(fn_data) = f.as_function() else {
            reject(self, "spawn: expected a function");
            return promise;
        };
        let Some(program) = self.programs.get(fn_data.program as usize) else {
            reject(self, "spawn: function's program is gone");
            return promise;
        };
        let program_bytes = match program.to_bytes() {
            Ok(b) => b,
            Err(_) => {
                reject(self, "spawn: cannot serialize function program");
                return promise;
            }
        };
        // Envelope: [program_len u32][program bytes][entry u32][upvalue
        // count u32][upvalue values...]. Upvalues are deref'd — their
        // contents are serialized, and the worker rebuilds fresh cells.
        let mut payload = Vec::with_capacity(program_bytes.len() + 64);
        payload.extend_from_slice(&(program_bytes.len() as u32).to_be_bytes());
        payload.extend_from_slice(&program_bytes);
        payload.extend_from_slice(&(fn_data.ptr as u32).to_be_bytes());
        payload.extend_from_slice(&(fn_data.cells.len() as u32).to_be_bytes());
        for c in &fn_data.cells {
            // Closures may capture other functions (a helper, a callback) —
            // allow them so the environment survives; they serialize against
            // the envelope's program.
            write_spawn_value(&mut payload, &c.borrow(), true, fn_data.program);
        }
        // Envelope tail: the spawn arguments, serialized like upvalues (a
        // closure may appear among them — e.g. passing a callback).
        payload.extend_from_slice(&(args.len() as u32).to_be_bytes());
        for a in args {
            write_spawn_value(&mut payload, a, true, fn_data.program);
        }
        let id = self.next_spawn_id;
        self.next_spawn_id += 1;
        self.spawn_inflight.insert(id, promise.clone());
        self.spawn_pending += 1;
        let tx = self.spawn_tx.clone();
        // The worker shares this VM's module registry — `require` inside the
        // spawned function reuses compiled bytes and honors cross-thread
        // `reload()` — and inherits the requirer's directory so relative
        // requires resolve like Node (against the calling file, not the cwd).
        let registry = self.registry.clone();
        let py_registry = self.py_registry.clone();
        let dir = self.current_dir.clone();
        match std::thread::Builder::new()
            .name(format!("alloy-spawn-{}", id))
            .spawn(move || spawn_worker_thread(payload, tx, id, registry, py_registry, dir))
        {
            Ok(h) => self.spawn_workers.push(h),
            Err(e) => {
                // Thread creation failed: settle immediately so the promise
                // never dangles.
                self.spawn_pending = self.spawn_pending.saturating_sub(1);
                self.spawn_inflight.remove(&id);
                reject(self, &format!("spawn: cannot start worker thread: {}", e));
            }
        }
        promise
    }

    /// Settle completed spawn workers and resolve their promises. Runs on the
    /// VM thread (the decoder allocates into the thread-local arena heap, and
    /// promises must settle where the event loop can pick up the enqueued
    /// microtasks).
    fn drain_spawn_completions(&mut self) {
        while let Ok((id, bytes)) = self.spawn_rx.try_recv() {
            self.spawn_pending = self.spawn_pending.saturating_sub(1);
            let Some(p) = self.spawn_inflight.remove(&id) else { continue };
            let Some(pr) = p.as_promise() else { continue };
            if bytes.is_empty() {
                continue;
            }
            let mut pos = 1;
            match bytes[0] {
                // 0 = fulfilled
                0 => match crate::bytecode::decode_value(&bytes, &mut pos) {
                    Ok(v) => self.resolve_promise(pr, v),
                    Err(_) => self.reject_promise(
                        pr,
                        Value::string("spawn: result decode failed".to_string()),
                    ),
                },
                // 1 = rejected
                1 => {
                    let v = crate::bytecode::decode_value(&bytes, &mut pos)
                        .unwrap_or_else(|_| Value::undefined());
                    self.reject_promise(pr, v);
                }
                // 2 = still pending (the spawned task parked on something
                // that cannot settle in the worker): leave the promise
                // pending, mirroring the parked task.
                _ => {}
            }
        }
    }

    /// Re-import check for a `.py` sidecar module: if a NEW reload burst
    /// started after the one this VM's pool was built in (a
    /// `reload('./x.py')` on ANY thread — main VM, spawn worker, async
    /// handler), tear the local pool down (kills the children) and rebuild
    /// from the current file on disk. Runs before every python call and
    /// module lookup, so a parked worker serves the fresh child on its next
    /// call instead of the stale one. Staleness is **burst**-based: only a
    /// reload that STARTED a new burst (outside the coalescing window)
    /// forces a rebuild; reloads folded into the current burst leave the
    /// pool alone so a burst of reloads never cascades into a burst of
    /// rebuilds.
    fn ensure_python_current(&mut self, src: &str) -> bool {
        let burst_now = self.py_registry.burst(src);
        let stale = match self.python_workers.get(src) {
            Some(w) => w.burst != burst_now,
            None => false,
        };
        if stale {
            // A new burst landed; drop this VM's pool + cached module object
            // so the rebuild below re-imports the file from disk.
            self.shutdown_python_worker(src);
            self.python_modules.remove(src);
        }
        if self.python_workers.get(src).is_none() {
            return self.start_python_worker(src, burst_now);
        }
        true
    }

    /// Start the worker **pool** for `src`'s sidecar. Only the first child
    /// spawns here; further children join lazily when every existing child
    /// has an in-flight call (`PythonWorker::send` -> `grow`), capped by
    /// `ALLOY_PYTHON_POOL`. Fails loudly (eprintln + false) when python or
    /// the file is bad. A rebuild (any start after the first import of this
    /// path) is counted in `python_rebuilds` for the coalescing test.
    fn start_python_worker(&mut self, src: &str, burst: u64) -> bool {
        if self.python_workers.contains_key(src) {
            return true;
        }
        if !self.python_started.insert(src.to_string()) {
            self.python_rebuilds += 1;
        }
        let (path, cap) = match self.shared.file_path() {
            Some(p) => (p.to_string_lossy().to_string(), self.shared.capacity()),
            None => {
                eprintln!("alloy python import error: shared segment is not file-backed");
                return false;
            }
        };
        // Embed mode is GIL-serialized: a pool of parallel in-process
        // interpreters can't run concurrently anyway, so cap it at one child
        // (same-file calls queue on its single worker). Child mode grows up
        // to `ALLOY_PYTHON_POOL` for same-file parallelism.
        let max = if crate::python_embed::embed_enabled() {
            1
        } else {
            std::env::var("ALLOY_PYTHON_POOL")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(PYTHON_POOL_SIZE)
                .max(1)
        };
        let mut worker = PythonWorker {
            senders: Vec::new(),
            busy: Vec::new(),
            next: 0,
            sidecars: Vec::new(),
            pids: Vec::new(),
            funcs: Vec::new(),
            handles: Vec::new(),
            max,
            path: path.clone(),
            cap,
            base: self.shared.raw_ptr() as usize,
            py_file: src.to_string(),
            complete: self.python_tx.clone(),
            burst,
            timeout: self.python_timeout,
        };
        if !worker.grow() {
            return false;
        }
        // The first child's handshake reported the file's functions.
        worker.funcs = worker
            .sidecars
            .first()
            .and_then(|sc| sc.lock().ok())
            .map(|s| s.funcs().to_vec())
            .unwrap_or_default();
        self.python_workers.insert(src.to_string(), worker);
        true
    }

    /// Return (building on first use) the module object for a `.py` import:
    /// one native per function the worker's sidecar reports. The handshake at
    /// import time validates the file loads, so broken imports fail loudly
    /// here instead of at the first call. Keys are **canonical paths** (the
    /// import's `./x.py` and `require`'s `/abs/x.py` name one pool, and
    /// `reload` matches by canonical path), and every lookup re-checks the
    /// shared burst so a reload that starts a new burst on any thread
    /// rebuilds the pool and the module object (fresh function names
    /// included) on the next use.
    fn python_module(&mut self, src: &str) -> Value {
        // Canonical when the file is on disk; raw (so the child's handshake
        // reports the failure loudly) otherwise.
        let canon = self
            .resolve_py_path(src)
            .unwrap_or_else(|| src.to_string());
        if !self.ensure_python_current(&canon) {
            return Value::undefined();
        }
        if let Some(v) = self.python_modules.get(&canon) {
            return v.clone();
        }
        let funcs = self
            .python_workers
            .get(&canon)
            .map(|w| w.funcs.clone())
            .unwrap_or_default();
        let base = self.shared.raw_ptr() as usize;
        let cap2 = self.shared.capacity();
        let mut m = hashbrown::HashMap::new();
        for f in funcs {
            let fname = f.clone();
            let src2 = canon.clone();
            let n = Value::native(Arc::new(move |args, vm| {
                vm.python_call(&src2, &fname, args, base, cap2)
            }));
            m.insert(f, n);
        }
        let obj = Value::object(m);
        self.python_modules.insert(canon, obj.clone());
        obj
    }

    /// Invoke a function in a `.py` sidecar without blocking the event loop:
    /// the wire request is built here (owned data only) and queued to the
    /// file's dedicated worker thread, while the VM returns immediately with
    /// a pending promise. When the response arrives (`python_rx`),
    /// `drain_python_completions` decodes it on the VM thread and settles the
    /// promise, so `await` resumes and a failed call rejects (which `await`
    /// throws). JS args are translated onto the wire first: shared-segment
    /// buffers (and raw in-segment pointers) become `p:` offsets — the
    /// sidecar reads the exact bytes at that offset, zero-copy.
    fn vm_python_call(
        &mut self,
        src: &str,
        func: &str,
        args: &[Value],
        base: usize,
        cap: usize,
    ) -> Value {
        let wake = self.wake_tx.clone();
        let reject = move |msg: String| {
            Value::promise(Arc::new(Mutex::new(PromiseState {
                status: PromiseStatus::Rejected(Value::string(msg)),
                continuations: Vec::new(),
                owner: Some(wake.clone()),
            })))
        };
        // Re-import check (a reload on any thread tears the pool down), then
        // queue the call to the file's worker pool; restart it if it died
        // (all children crashed) so the module keeps working.
        if !self.ensure_python_current(src) {
            return reject(format!("python module '{}' is not loaded", src));
        }
        let wire: Vec<PyArg> = args.iter().map(|a| python_arg(a, base, cap)).collect();
        let line = PythonSidecar::build_line(func, &wire);
        let promise = self.new_promise();
        let id = self.next_cont_id;
        self.next_cont_id += 1;
        // Stamp the call with the reload burst the pool was just (re)built
        // in: if a NEW burst starts before this response arrives, the
        // response is discarded and the call re-runs on the fresh child.
        let burst = self.py_registry.burst(src);
        self.python_inflight += 1;
        self.python_inflight_calls.insert(
            id,
            InflightPyCall {
                promise: promise.clone(),
                burst,
                line: line.clone(),
            },
        );
        if !self.python_send(src, PyRequest { id, line }) {
            // Every worker exited between lookup and send: drop it, reject.
            self.python_workers.remove(src);
            self.python_inflight = self.python_inflight.saturating_sub(1);
            self.python_inflight_calls.remove(&id);
            return reject("python sidecar is not running".to_string());
        }
        promise
    }

    /// Round-robin a request onto one of `src`'s pool children.
    fn python_send(&mut self, src: &str, req: PyRequest) -> bool {
        match self.python_workers.get_mut(src) {
            Some(w) => w.send(req),
            None => false,
        }
    }

    /// Drain completed python sidecar calls and settle their promises. Runs
    /// on the VM thread (the wire decoder allocates into the thread-local
    /// arena heap, and promises must resolve where the event loop can pick up
    /// the enqueued microtasks). Returns whether anything was resolved.
    ///
    /// Per-call burst check: each call carries the shared `.py` reload burst
    /// it was queued in. If the registry moved past it by the time the
    /// response arrives, a NEW burst of reloads landed on ANY thread while
    /// the call was in flight — the result came from old code (the reload
    /// already killed the child it ran on). The pool is rebuilt from the
    /// current file and the call is re-run on the fresh child, so the
    /// promise resolves with the current implementation instead of settling
    /// stale or rejecting. Reloads folded into the same burst don't move the
    /// stamp, so a burst of reloads causes exactly one re-run, not a
    /// cascade.
    fn drain_python_completions(&mut self) -> bool {
        let mut any = false;
        while let Ok((src, idx, id, resp)) = self.python_rx.try_recv() {
            any = true;
            self.python_inflight = self.python_inflight.saturating_sub(1);
            // Free the child's in-flight slot so the pool can grow back down
            // and its busy counts stay accurate.
            if let Some(w) = self.python_workers.get_mut(&src) {
                if idx < w.busy.len() {
                    w.busy[idx] = w.busy[idx].saturating_sub(1);
                }
            }
            let Some(mut call) = self.python_inflight_calls.remove(&id) else {
                continue;
            };
            let burst_now = self.py_registry.burst(&src);
            if burst_now != call.burst {
                // Stale: a new burst's reload already tore the pool down (or
                // this thread's pool is stale — rebuild kills its children
                // too), so rebuild from the current file and re-run the
                // call.
                self.python_inflight += 1;
                if !self.ensure_python_current(&src) {
                    self.python_inflight = self.python_inflight.saturating_sub(1);
                    if let Some(pr) = call.promise.as_promise() {
                        self.reject_promise(
                            pr,
                            Value::string(format!("python module '{}' is not loaded", src)),
                        );
                    }
                    continue;
                }
                let burst2 = self.py_registry.burst(&src);
                call.burst = burst2;
                self.python_inflight_calls.insert(id, call.clone());
                if !self.python_send(&src, PyRequest { id, line: call.line.clone() }) {
                    // Fresh pool died between build and send: drop, reject.
                    self.python_inflight = self.python_inflight.saturating_sub(1);
                    self.python_inflight_calls.remove(&id);
                    if let Some(pr) = call.promise.as_promise() {
                        self.reject_promise(pr, Value::string("python sidecar is not running".to_string()));
                    }
                }
                continue;
            }
            let promise = call.promise;
            if let Some(pr) = promise.as_promise() {
                if let Some(rest) = resp.strip_prefix("ok ") {
                    let mut i = 0;
                    match crate::python_sidecar::decode_wire(rest.as_bytes(), &mut i) {
                        Ok(v) => self.resolve_promise(pr, v),
                        Err(e) => self.reject_promise(pr, Value::string(e)),
                    }
                } else if let Some(rest) = resp.strip_prefix("err ") {
                    let msg = crate::python_sidecar::unescape(rest);
                    self.reject_promise(pr, Value::string(msg));
                } else {
                    self.reject_promise(
                        pr,
                        Value::string(format!("python sidecar unexpected response: {}", resp)),
                    );
                }
            }
        }
        any
    }

    fn dispatch(&mut self, mut pc: usize) -> Value {
        let mut budget: u64 = self.instruction_budget.unwrap_or(u64::MAX);
        let has_budget = self.instruction_budget.is_some();
        loop {
            if pc >= self.bytecode.len() {
                break;
            }
            if has_budget {
                if budget == 0 {
                    self.budget_exhausted = true;
                    break;
                }
                budget -= 1;
            }

            // Hot loop uses unchecked byte fetch; length checked at top.
            // SAFETY: pc < len checked above, so index is in-bounds.
            let op_byte = unsafe { *self.bytecode.get_unchecked(pc) };
            // Cached flag (set once at Vm construction) — no OnceLock/mutex
            // traffic on the hot path when profiling is off.
            if self.op_hist_on {
                if let Some(h) = op_hist() {
                    if let Ok(mut g) = h.lock() {
                        g[op_byte as usize] += 1;
                    }
                }
            }
            // Back-edge counter for the baseline-JIT hypervisor: only taken
            // backwards jumps pay the HashMap increment.
            // (Forward jumps and fall-through cost one predictable branch.)
            let op = match Opcode::from_u8(op_byte) {
                Some(o) => o,
                None => { pc += 1; continue; }
            };
            match op {
                Opcode::Halt => break,

                Opcode::Nop => { pc += 1; }

                Opcode::LoadConst => {
                    let idx = self.read_u16(pc + 1);
                    let val = self.constants[idx as usize].clone();
                    self.push(val);
                    pc += 3;
                }
                Opcode::LoadInt => {
                    let val = self.read_u32(pc + 1) as i64;
                    self.push(Value::int(val));
                    pc += 5;
                }
                Opcode::LoadTrue => { self.push(Value::bool(true)); pc += 1; }
                Opcode::LoadFalse => { self.push(Value::bool(false)); pc += 1; }
                Opcode::LoadNull => { self.push(Value::null()); pc += 1; }
                Opcode::LoadUndefined => { self.push(Value::undefined()); pc += 1; }

                Opcode::LoadLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let val = if idx < self.stack.len() {
                        // Fast: a slot known to hold a direct int/number is
                        // never a cell — raw word, no probe, no clone dispatch.
                        match self.stack.kind_of(idx) {
                            KIND_INT | KIND_NUMBER => {
                                Value::from_raw_word(self.stack.at(idx).bits())
                            }
                            _ => {
                                let v = self.stack.at(idx).clone();
                                match v.as_cell() {
                                    Some(c) => c.borrow().clone(),
                                    None => v,
                                }
                            }
                        }
                    } else {
                        Value::undefined()
                    };
                    self.push(val);
                    pc += 2;
                }
                Opcode::StoreLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    self.store_slot(base + slot, val);
                    pc += 2;
                }
                Opcode::LoadGlobal => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let missing = idx >= self.globals.len()
                        || idx >= self.global_defined.len()
                        || !self.global_defined[idx];
                    if missing {
                        // JS: reading an undeclared identifier is a
                        // ReferenceError (a `let` that was never assigned is
                        // defined; a name that was never declared is not).
                        let name = self
                            .programs[self.program_id as usize]
                            .globals
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(|| "<unknown>".to_string());
                        match self.throw_value(Value::string(format!(
                            "ReferenceError: {} is not defined",
                            name
                        ))) {
                            ThrowResult::Jump(p) => pc = p,
                            ThrowResult::EndDispatch => break,
                            ThrowResult::Abort => break,
                        }
                        continue;
                    }
                    // A slot may hold a live-import cell (module exports
                    // aliased into this scope): read through to the current
                    // value so imported bindings observe later mutations.
                    let v = self.globals[idx].clone();
                    let v = match v.as_cell() {
                        Some(c) => c.borrow().clone(),
                        None => v,
                    };
                    self.push(v);
                    pc += 3;
                }
                Opcode::TypeOfGlobal => {
                    // `typeof g` on a never-declared global is "undefined",
                    // not a ReferenceError.
                    let idx = self.read_u16(pc + 1) as usize;
                    let v = if idx < self.globals.len() {
                        self.globals[idx].clone()
                    } else {
                        Value::undefined()
                    };
                    self.push(Value::string(v.type_name().to_string()));
                    pc += 3;
                }
                Opcode::StoreGlobal => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let val = self.pop();
                    if idx >= self.globals.len() {
                        self.globals.resize_with(idx + 1, || Value::undefined());
                    }
                    // A live-import cell in the slot (a module export aliased
                    // into this scope, or the module's own exported global):
                    // write through to the shared cell so every alias — the
                    // exports object, other importers, the module itself —
                    // sees the new value. The cell stays in the slot.
                    match self.globals[idx].as_cell() {
                        Some(c) => *c.borrow_mut() = val.clone(),
                        None => self.globals[idx] = val.clone(),
                    }
                    if idx >= self.global_defined.len() {
                        self.global_defined.resize_with(idx + 1, || false);
                    }
                    self.global_defined[idx] = true;
                    // Mirror into the stable table so every program sees it —
                    // but NOT for modules: their globals live in the isolated
                    // `modules` view and must never leak into the requirer's
                    // namespace (or vice versa).
                    if !self.modules.contains_key(&self.program_id) {
                        if let Some(name) = self
                            .programs[self.program_id as usize]
                            .globals
                            .get(idx)
                            .cloned()
                        {
                            if let Some(si) = self.global_names.iter().position(|n| *n == name) {
                                self.stable_globals[si] = val;
                                if si >= self.stable_defined.len() {
                                    self.stable_defined.resize_with(si + 1, || false);
                                }
                                self.stable_defined[si] = true;
                            }
                        }
                    }
                    pc += 3;
                }

                Opcode::Add => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.add(&r));
                    pc += 1;
                }
                Opcode::Subtract => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.subtract(&r));
                    pc += 1;
                }
                Opcode::Multiply => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.multiply(&r));
                    pc += 1;
                }
                Opcode::Divide => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.divide(&r));
                    pc += 1;
                }
                Opcode::Modulo => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.modulo(&r));
                    pc += 1;
                }
                Opcode::Negate => {
                    let val = self.pop();
                    self.push(val.negate());
                    pc += 1;
                }

                Opcode::Equal => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.equal(&r));
                    pc += 1;
                }
                Opcode::NotEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(!l.equal(&r).is_truthy()));
                    pc += 1;
                }
                Opcode::StrictEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(strict_equal(&l, &r)));
                    pc += 1;
                }
                Opcode::StrictNotEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(!strict_equal(&l, &r)));
                    pc += 1;
                }
                Opcode::BitAnd => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitand(&r));
                    pc += 1;
                }
                Opcode::BitOr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitor(&r));
                    pc += 1;
                }
                Opcode::BitXor => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitxor(&r));
                    pc += 1;
                }
                Opcode::BitNot => {
                    let v = self.pop();
                    self.push(v.bitnot());
                    pc += 1;
                }
                Opcode::Shl => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.shl(&r));
                    pc += 1;
                }
                Opcode::Shr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.shr(&r));
                    pc += 1;
                }
                Opcode::UShr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.ushr(&r));
                    pc += 1;
                }
                Opcode::Pow => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.pow(&r));
                    pc += 1;
                }
                Opcode::DeleteProp => {
                    let name = self.pop();
                    let obj = self.pop();
                    if let Some(m) = obj.as_object() {
                        if let Some(n) = name.as_str() {
                            m.borrow_mut().delete(n);
                        }
                    }
                    // JS: deleting a property (existing or not) is true.
                    self.push(Value::bool(true));
                    pc += 1;
                }
                Opcode::DeleteIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    if let Some(a) = obj.as_array() {
                        let i = idx.to_number();
                        if i.is_finite() && i >= 0.0 {
                            let mut a = a.borrow_mut();
                            let ix = i as usize;
                            if ix < a.len() {
                                // Deleting an element writes a hole, which
                                // escapes the packed form (undefined is not
                                // representable as an int).
                                a.set(ix, Value::undefined());
                            }
                        }
                    } else if let Some(m) = obj.as_object() {
                        let key = match idx.as_str() {
                            Some(k) => k.to_string(),
                            None => format!("{}", idx),
                        };
                        m.borrow_mut().delete(&key);
                    }
                    self.push(Value::bool(true));
                    pc += 1;
                }
                // JS semantics: numeric relational comparison, except when both
                // operands are strings, which compare lexicographically.
                Opcode::Less => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a < b
                    } else {
                        l.to_number() < r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::Greater => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a > b
                    } else {
                        l.to_number() > r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::LessEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a <= b
                    } else {
                        l.to_number() <= r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::GreaterEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a >= b
                    } else {
                        l.to_number() >= r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }

                // ---- Fused superinstructions ----

                Opcode::CmpLocalInt => {
                    // r{slot} cmp imm : one dispatch for `i < 1000` style.
                    // When the slot's feedback kind is INT/NUMBER, the
                    // comparison runs in the raw i64/f64 lane — no tag
                    // probes, no ToNumber (collatz's `n !== 1` on an f64 `n`
                    // is the canonical case).
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp = self.bytecode[pc + 6];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = if idx < self.stack.len() {
                        match self.stack.kind_of(idx) {
                            KIND_INT => cmp_i64(Value::int_bits_raw(self.stack.at(idx).bits()), imm, cmp),
                            KIND_NUMBER => cmp_f64(f64::from_bits(self.stack.at(idx).bits()), imm as f64, cmp),
                            _ => compare_values(&self.slot_value(idx), &Value::int(imm), cmp),
                        }
                    } else {
                        compare_values(&Value::undefined(), &Value::int(imm), cmp)
                    };
                    self.push(Value::bool(result));
                    pc += 7;
                }
                Opcode::BinLocalInt => {
                    // r{slot} ar imm : one dispatch for `i * 3` style. The
                    // peephole folds a trailing Pop into bit 7 of ar (keep=0).
                    // Known INT/NUMBER slots run in the raw lane (collatz's
                    // `n % 2` on an f64 `n` skips the whole probe storm).
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let ar = self.bytecode[pc + 6];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_local_imm(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let l = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&l, &Value::int(imm), ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::BinLocalLocal => {
                    // r{a} ar r{b} : one dispatch for `i + j` style. The
                    // peephole folds a trailing Pop into bit 7 of ar (keep=0).
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let ar = self.bytecode[pc + 3];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (result, fast) = alu_local_local(&self.stack, base + a, base + b, ar);
                    let result = if fast {
                        result
                    } else {
                        let va = if base + a < self.stack.len() {
                            self.slot_value(base + a)
                        } else {
                            Value::undefined()
                        };
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&va, &vb, ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 4;
                }
                Opcode::BinIntLocal => {
                    // imm ar r{slot} : one dispatch for `3 * n` style
                    // int-on-left patterns (peephole: LoadInt+LoadLocal+AR).
                    // Bit 7 of ar = keep=0 (folded trailing Pop).
                    let imm = self.read_i32(pc + 1) as i64;
                    let slot = self.bytecode[pc + 5] as usize;
                    let ar = self.bytecode[pc + 6];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_imm_local(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let r = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&Value::int(imm), &r, ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::BinLocalLocalInt => {
                    // (r{a} ar1 r{b}) ar2 imm : one dispatch for `(i + j) % 7`
                    // style chains (peephole: BinLocalLocal+LoadInt+AR). Bit 7
                    // of ar2 = keep=0 (folded trailing Pop).
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let ar1 = self.bytecode[pc + 3];
                    let imm = self.read_i32(pc + 4) as i64;
                    let ar2 = self.bytecode[pc + 8];
                    let keep = ar2 & 0x80 == 0;
                    let ar2 = ar2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (mid, fast) = alu_local_local(&self.stack, base + a, base + b, ar1);
                    let mid = if fast {
                        mid
                    } else {
                        let va = if base + a < self.stack.len() {
                            self.slot_value(base + a)
                        } else {
                            Value::undefined()
                        };
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&va, &vb, ar1)
                    };
                    let result = arith_apply(&mid, &Value::int(imm), ar2);
                    if keep {
                        self.push(result);
                    }
                    pc += 9;
                }
                Opcode::ArithChain => {
                    // Register-ALU chain: ONE dispatch for an int-arithmetic
                    // tree (`3 * n + 1`, `(lo + hi) % 2`, `seed = (seed *
                    // 48271) % 2147483648`, `x op= chain`). The running value
                    // stays in an i64 register, boxed once at the end (or not
                    // at all when the terminal stores straight to a local).
                    // Non-int values and bitwise/shift/pow steps fall back to
                    // the generic Value path per step, so semantics are
                    // exactly those of the plain opcode sequence.
                    //
                    // Lean decode: the step bytes are copied once (a single
                    // bounds-checked slice read) into a stack array, then
                    // decoded with unchecked indexing — a per-step
                    // `read_u32` (4 bounds-checked loads) was ~3x the cost of
                    // the arithmetic itself and made the fusion a net loss.
                    let count = self.bytecode[pc + 1] as usize;
                    let term = self.bytecode[pc + 2];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let n = count * 5;
                    let mut ops = [0u8; 125];
                    ops[..n]
                        .copy_from_slice(&self.bytecode[pc + 3..pc + 3 + n]);
                    let mut j = 0;
                    let mut acc_i: i64 = 0;
                    let mut acc: Value = Value::undefined();
                    let mut ok = true; // acc held in acc_i
                    while j < count {
                        let h = ops[j * 5];
                        // enc_ar = arith_code + 1; 0 = init marker (a + b must
                        // not collide with the init step's ar=0).
                        let enc_ar = h & 0x1F;
                        let kind = h >> 5;
                        let u = ((ops[j * 5 + 1] as u32) << 24)
                            | ((ops[j * 5 + 2] as u32) << 16)
                            | ((ops[j * 5 + 3] as u32) << 8)
                            | (ops[j * 5 + 4] as u32);
                        let imm = if u & 0x8000_0000 != 0 { u as i32 as i64 } else { u as i64 };
                        j += 1;
                        match kind {
                            0 => {
                                // LoadLocal.
                                let idx = base + (u & 0xFF) as usize;
                                let mut v = if idx < self.stack.len() {
                                    self.stack.at(idx).clone()
                                } else {
                                    Value::undefined()
                                };
                                let deref = v.as_cell().map(|c| c.borrow().clone());
                                if let Some(val) = deref {
                                    v = val;
                                }
                                if enc_ar == 0 {
                                    // Init: acc = v.
                                    if ok {
                                        if let Some(b) = v.as_int() {
                                            acc_i = b;
                                            continue;
                                        }
                                    }
                                    acc = v;
                                    ok = false;
                                } else if ok {
                                    let ar = enc_ar - 1;
                                    if let Some(b) = v.as_int() {
                                        if let Some(res) = chain_arith_i64(acc_i, b, ar) {
                                            match res.as_int() {
                                                Some(r) => acc_i = r,
                                                None => {
                                                    acc = res;
                                                    ok = false;
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    acc = arith_apply(&Value::int(acc_i), &v, ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&acc, &v, enc_ar - 1);
                                }
                            }
                            1 => {
                                // Const: the i64 is already in hand — no
                                // boxing round-trip through Value.
                                if enc_ar == 0 {
                                    acc_i = imm;
                                    ok = true;
                                } else if ok {
                                    let ar = enc_ar - 1;
                                    if let Some(res) = chain_arith_i64(acc_i, imm, ar) {
                                        match res.as_int() {
                                            Some(r) => acc_i = r,
                                            None => {
                                                acc = res;
                                                ok = false;
                                            }
                                        }
                                        continue;
                                    }
                                    acc = arith_apply(&Value::int(acc_i), &Value::int(imm), ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&acc, &Value::int(imm), enc_ar - 1);
                                }
                            }
                            2 => {
                                // Save: push the current acc.
                                let v = if ok { Value::int(acc_i) } else { acc.clone() };
                                self.push(v);
                            }
                            _ => {
                                // Combine: acc = t ar acc.
                                let ar = enc_ar - 1;
                                let t = self.pop();
                                if ok {
                                    if let Some(b) = t.as_int() {
                                        if let Some(res) = chain_arith_i64(b, acc_i, ar) {
                                            match res.as_int() {
                                                Some(r) => acc_i = r,
                                                None => {
                                                    acc = res;
                                                    ok = false;
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    acc = arith_apply(&t, &Value::int(acc_i), ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&t, &acc, ar);
                                }
                            }
                        }
                    }
                    let result = if ok { Value::int(acc_i) } else { acc };
                    if term & 0x40 != 0 {
                        self.store_slot(base + (term & 0x3F) as usize, result.clone());
                    }
                    if term & 0x80 != 0 {
                        self.push(result);
                    }
                    pc = pc + 3 + n;
                }
                Opcode::Arith2StoreLocalConst => {
                    // t = locals[slot] ar imm; store back to slot — ONE
                    // dispatch for `n = n / 2`, `j -= 1`, `steps += 1`. Bit 7
                    // of ar = keep (the assignment's value is pushed). A
                    // known INT/NUMBER slot runs the whole op in the raw
                    // lane: no as_cell probe, no as_int probe, no probe on
                    // the store.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar_byte = self.bytecode[pc + 2];
                    let keep = ar_byte & 0x80 != 0;
                    let ar = ar_byte & 0x7F;
                    let imm = self.read_i32(pc + 3) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_local_imm(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let mut v = if idx < self.stack.len() {
                            self.stack.at(idx).clone()
                        } else {
                            Value::undefined()
                        };
                        let deref = v.as_cell().map(|c| c.borrow().clone());
                        if let Some(val) = deref {
                            v = val;
                        }
                        chain_step_i64(&v, imm, ar)
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::Arith3StoreLocalConstConst => {
                    // t = (locals[slot] ar1 imm1) ar2 imm2; store slot — ONE
                    // dispatch for `seed = (seed * 48271) % 2147483648`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar1 = self.bytecode[pc + 2];
                    let imm1 = self.read_i32(pc + 3) as i64;
                    let ar2_byte = self.bytecode[pc + 7];
                    let keep = ar2_byte & 0x80 != 0;
                    let ar2 = ar2_byte & 0x7F;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = match alu2_local_imm_imm(&self.stack, idx, imm1, ar1, imm2, ar2) {
                        Some(r) => r,
                        None => {
                            let mut v = if idx < self.stack.len() {
                                self.stack.at(idx).clone()
                            } else {
                                Value::undefined()
                            };
                            let deref = v.as_cell().map(|c| c.borrow().clone());
                            if let Some(val) = deref {
                                v = val;
                            }
                            let mut result = chain_step_i64(&v, imm1, ar1);
                            result = chain_step_i64(&result, imm2, ar2);
                            result
                        }
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 12;
                }
                Opcode::Arith3StoreConstLocalConst => {
                    // t = (imm1 ar1 locals[slot]) ar2 imm2; store slot — ONE
                    // dispatch for `n = 3 * n + 1` (constant init on the
                    // left, so non-commutative ar1 still applies correctly).
                    let imm1 = self.read_i32(pc + 1) as i64;
                    let ar1 = self.bytecode[pc + 5];
                    let slot = self.bytecode[pc + 6] as usize;
                    let ar2_byte = self.bytecode[pc + 7];
                    let keep = ar2_byte & 0x80 != 0;
                    let ar2 = ar2_byte & 0x7F;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = match alu2_imm_local_imm(&self.stack, idx, imm1, ar1, imm2, ar2) {
                        Some(r) => r,
                        None => {
                            let mut v = if idx < self.stack.len() {
                                self.stack.at(idx).clone()
                            } else {
                                Value::undefined()
                            };
                            let deref = v.as_cell().map(|c| c.borrow().clone());
                            if let Some(val) = deref {
                                v = val;
                            }
                            let l = Value::int(imm1);
                            let mut result = if let Some(a) = v.as_int() {
                                chain_step_i64(&l, a, ar1)
                            } else {
                                arith_apply(&l, &v, ar1)
                            };
                            result = chain_step_i64(&result, imm2, ar2);
                            result
                        }
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 12;
                }
                Opcode::CmpLocalLocal => {
                    // r{a} cmp r{b} : one dispatch for `lo <= hi` style.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp = self.bytecode[pc + 3];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let ia = base + a;
                    let ib = base + b;
                    let result = if ia < self.stack.len() && ib < self.stack.len() {
                        match (self.stack.kind_of(ia), self.stack.kind_of(ib)) {
                            (KIND_INT, KIND_INT) => cmp_i64(
                                Value::int_bits_raw(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_INT, KIND_NUMBER) => cmp_f64(
                                Value::int_bits_raw(self.stack.at(ia).bits()) as f64,
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_INT) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()) as f64,
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_NUMBER) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            _ => {
                                compare_values(&self.slot_value(ia), &self.slot_value(ib), cmp)
                            }
                        }
                    } else {
                        let va = if ia < self.stack.len() { self.slot_value(ia) } else { Value::undefined() };
                        let vb = if ib < self.stack.len() { self.slot_value(ib) } else { Value::undefined() };
                        compare_values(&va, &vb, cmp)
                    };
                    self.push(Value::bool(result));
                    pc += 4;
                }
                Opcode::CmpAndLocalLocal => {
                    // r{a} cmp1 r{b} &&/|| r{c} cmp2 r{d} : one dispatch for
                    // `a < b && b < c`. The short-circuit value semantics are
                    // preserved: when the first bool fires (&&: falsy, ||:
                    // truthy) it IS the result and the second comparison is
                    // never evaluated. Bit 7 of cmp2 selects `||`.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp1 = self.bytecode[pc + 3];
                    let c = self.bytecode[pc + 4] as usize;
                    let d = self.bytecode[pc + 5] as usize;
                    let cmp2 = self.bytecode[pc + 6];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    let vb = if base + b < self.stack.len() {
                        self.slot_value(base + b)
                    } else {
                        Value::undefined()
                    };
                    let v1 = compare_values(&va, &vb, cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        let vd = if base + d < self.stack.len() {
                            self.slot_value(base + d)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &vd, cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 7;
                }
                Opcode::CmpAndLocalInt => {
                    // r{a} cmp1 r{b} &&/|| r{c} cmp2 imm : one dispatch for
                    // `a < b && b < 5` style mixed chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp1 = self.bytecode[pc + 3];
                    let c = self.bytecode[pc + 4] as usize;
                    let imm = self.read_i32(pc + 5) as i64;
                    let cmp2 = self.bytecode[pc + 9];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    let vb = if base + b < self.stack.len() {
                        self.slot_value(base + b)
                    } else {
                        Value::undefined()
                    };
                    let v1 = compare_values(&va, &vb, cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &Value::int(imm), cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 10;
                }
                Opcode::CmpAndIntLocal => {
                    // imm cmp1 r{a} &&/|| r{c} cmp2 r{d} : one dispatch for
                    // `5 < a && b < c` style mixed chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp1 = self.bytecode[pc + 6];
                    let c = self.bytecode[pc + 7] as usize;
                    let d = self.bytecode[pc + 8] as usize;
                    let cmp2 = self.bytecode[pc + 9];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    // CmpLocalInt semantics: r{slot} cmp imm (the slot is the
                    // LEFT operand — `<` is not symmetric).
                    let v1 = compare_values(&va, &Value::int(imm), cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        let vd = if base + d < self.stack.len() {
                            self.slot_value(base + d)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &vd, cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 10;
                }
                Opcode::CmpAndIntInt => {
                    // imm1 cmp1 r{a} &&/|| r{b} cmp2 imm2 : one dispatch for
                    // `5 < a && a < 10` style chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let imm1 = self.read_i32(pc + 2) as i64;
                    let cmp1 = self.bytecode[pc + 6];
                    let b = self.bytecode[pc + 7] as usize;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let cmp2 = self.bytecode[pc + 12];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    // CmpLocalInt semantics: r{slot} cmp imm on both sides.
                    let v1 = compare_values(&va, &Value::int(imm1), cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vb, &Value::int(imm2), cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 13;
                }
                Opcode::ArithStoreLocal => {
                    // [lhs, rhs] -> store (lhs ar rhs) into r{slot}, keep?
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar = self.bytecode[pc + 2];
                    let keep = self.bytecode[pc + 3];
                    let r = self.pop();
                    let l = self.pop();
                    let res = arith_apply(&l, &r, ar);
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    self.store_slot(base + slot, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }
                Opcode::AppendStringConst => {
                    // s = s + "x" / s += "x": one dispatch reads the local,
                    // adds the folded constant (exact `Add` semantics), stores
                    // back, and pushes the result if keep. The builder box
                    // never leaves the local slot.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ci = self.read_u16(pc + 2) as usize;
                    let keep = self.bytecode[pc + 4];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let l = self.slot_value(idx);
                    let r = self.constants[ci].clone();
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 5;
                }
                Opcode::AppendStringLocal => {
                    // s = s + t / s += t: both locals read inside the opcode.
                    let slot = self.bytecode[pc + 1] as usize;
                    let src = self.bytecode[pc + 2] as usize;
                    let keep = self.bytecode[pc + 3];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let l = self.slot_value(idx);
                    let r = self.slot_value(base + src);
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }
                Opcode::AppendStringPop => {
                    // s = s + <expr>: the lhs snapshot was pushed before the
                    // RHS evaluated (JS order); pop both, add, store.
                    let slot = self.bytecode[pc + 1] as usize;
                    let keep = self.bytecode[pc + 2];
                    let r = self.pop();
                    let l = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 3;
                }
                Opcode::IncLocal => {
                    // x++ / ++x / x-- / --x on a local: read, mutate, store,
                    // push old (postfix) or new (prefix) if keep.
                    let slot = self.bytecode[pc + 1] as usize;
                    let flags = self.bytecode[pc + 2];
                    let delta = self.bytecode[pc + 3] as i8;
                    let prefix = flags & 1 != 0;
                    let keep = flags & 2 != 0;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let ar = if delta > 0 { 0u8 } else { 1u8 };
                    let (nv, fast) = alu_local_imm(&self.stack, idx, 1, ar);
                    let (v, nv) = if fast {
                        // Old value is the raw slot word (postfix needs it
                        // before the store below overwrites it).
                        (self.slot_value(idx), nv)
                    } else {
                        let v = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        (v.clone(), arith_apply(&v, &Value::int(1), ar))
                    };
                    self.store_slot(idx, nv.clone());
                    if keep {
                        self.push(if prefix { nv } else { v });
                    }
                    pc += 4;
                }
                Opcode::ArithStoreUpvalue => {
                    // [lhs, rhs] -> write (lhs ar rhs) into upvalue u, keep?
                    let up = self.bytecode[pc + 1] as usize;
                    let ar = self.bytecode[pc + 2];
                    let keep = self.bytecode[pc + 3];
                    let r = self.pop();
                    let l = self.pop();
                    let res = arith_apply(&l, &r, ar);
                    let cell = self.cells_stack.last().and_then(|c| c.get(up)).cloned();
                    if let Some(cell) = cell {
                        self.note_rc_dirty(RcDirtyRef::Cell(cell.clone()));
                        *cell.borrow_mut() = res.clone();
                    }
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }

                Opcode::And => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(l.is_truthy() && r.is_truthy()));
                    pc += 1;
                }
                Opcode::Or => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(l.is_truthy() || r.is_truthy()));
                    pc += 1;
                }
                Opcode::Not => {
                    let val = self.pop();
                    self.push(Value::bool(!val.is_truthy()));
                    pc += 1;
                }

                Opcode::Jump => {
                    let target = self.read_u32(pc + 1);
                    let t = target as usize;
                    // Hot-loop hypervisor: count taken back-edges only.
                    if t < pc {
                        let c = self.backedge_counts.entry(t).or_insert(0);
                        *c = c.saturating_add(1);
                        if *c == 50_000 && std::env::var("ALLOY_JIT_LOG").is_ok() {
                            eprintln!("[alloy-jit] hot loop pc={:04x} trips={}", t, *c);
                        }
                    }
                    pc = t;
                }
                Opcode::JumpIfFalse => {
                    let val = self.peek();
                    let target = self.read_u32(pc + 1);
                    if !val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfFalsePop => {
                    // Consumes the condition it tests (loop conditions, if /
                    // ternary tests discard it on both paths).
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if !val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfTruePop => {
                    // Mirror of JumpIfFalsePop for inline `||` short-circuits
                    // in loop conditions: pops the operand and jumps to the
                    // loop body when it is truthy.
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfTrue => {
                    let val = self.peek();
                    let target = self.read_u32(pc + 1);
                    if val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfNullish => {
                    // Optional chaining: consumes the tested value and jumps
                    // to the short-circuit path when it is null or undefined
                    // (the chain discards its accumulated values and pushes
                    // undefined there).
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if val.is_null() || val.is_undefined() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }

                Opcode::ToIterable => {
                    // Synthetic iterator: a Map yields its [k, v] entry pairs,
                    // a Set its elements (insertion order). Arrays and
                    // strings are already iterable and pass through.
                    // Anything else is not iterable — throw Node's TypeError
                    // instead of silently iterating zero times.
                    let v = self.pop();
                    let is_container = match v.as_object() {
                        Some(od) => od.borrow().container != 0,
                        None => false,
                    };
                    if !(v.is_array() || v.as_str().is_some() || is_container) {
                        match self.throw_value(Value::string(format!(
                            "TypeError: {} is not iterable",
                            iterable_display(&v)
                        ))) {
                            ThrowResult::Jump(p) => pc = p,
                            ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                        }
                        continue;
                    }
                    if is_container {
                        match v.as_object().map(|od| od.borrow().container) {
                            Some(1) => self.push(container_entries(&v)),
                            Some(2) => self.push(container_values(&v)),
                            _ => self.push(v),
                        }
                    } else {
                        self.push(v);
                    }
                    pc += 1;
                }

                Opcode::MakeRegex => {
                    // /pattern/flags — push a fresh regex value. The pattern
                    // and flags are string constants; the compiled program is
                    // cached per (pattern, flags) and shared (Arc). The lexer
                    // already validated the pattern at compile time, so this
                    // is a cache hit; hand-crafted bytecode that missed
                    // validation throws a catchable SyntaxError instead.
                    let pi = self.read_u16(pc + 1) as usize;
                    let fi = self.read_u16(pc + 3) as usize;
                    let pattern = match self.constants.get(pi) {
                        Some(v) => v.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    };
                    let flags = match self.constants.get(fi) {
                        Some(v) => v.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    };
                    let key = (pattern, flags);
                    let compiled = match self.regex_cache.get(&key) {
                        Some(c) => c.clone(),
                        None => match regex::compile_from_str(&key.0, &key.1) {
                            Ok(c) => {
                                let c = Arc::new(c);
                                self.regex_cache.insert(key, c.clone());
                                c
                            }
                            Err(e) => {
                                match self.throw_value(Value::string(format!(
                                    "SyntaxError: invalid regular expression: {e}"
                                ))) {
                                    ThrowResult::Jump(p) => pc = p,
                                    ThrowResult::EndDispatch | ThrowResult::Abort => {
                                        pc = usize::MAX
                                    }
                                }
                                continue;
                            }
                        },
                    };
                    self.push(Value::regex(compiled));
                    pc += 5;
                }

                Opcode::Call | Opcode::CallKeep0 => {
                    let argc = self.bytecode[pc + 1] as usize;
                    let callee = self.pop();
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        matches!(op, Opcode::Call),
                        None,
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::CallMethod | Opcode::CallMethodKeep0 => {
                    // `o.m(args)`: the compiler emitted [receiver, func, args]
                    // — args on TOP, so pop them first, then the func. The
                    // receiver then sits at stack.len()-1 (base-1 after the
                    // args are re-pushed), exactly the frame's this slot.
                    let argc = self.bytecode[pc + 1] as usize;
                    let mut args: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    args.reverse();
                    let callee = self.pop();
                    let this_slot = self.stack.len().saturating_sub(1);
                    for a in args {
                        self.push(a);
                    }
                    let keep = matches!(op, Opcode::CallMethod);
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        keep,
                        Some(this_slot),
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the callee body runs. JS functions clean up in Return;
                    // natives clean up inside dispatch_call.
                }
                Opcode::CallMethodSpread | Opcode::CallMethodSpreadKeep0 => {
                    // `o.m(...args)`: same layout as CallMethod, with the
                    // spread positions expanded first.
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let callee = self.pop();
                    let this_slot = self.stack.len().saturating_sub(1);
                    let args = expand_spreads(vals, mask);
                    for a in &args {
                        self.push(a.clone());
                    }
                    let keep = matches!(op, Opcode::CallMethodSpread);
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        keep,
                        Some(this_slot),
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the callee body runs. JS functions clean up in Return;
                    // natives clean up inside dispatch_call.
                }
                Opcode::New => {
                    // `new C(args)`: build the instance (proto = C.prototype),
                    // put it below the args, and call the constructor with
                    // `this` bound — the frame's ctor-return semantics keep
                    // the instance unless the constructor returns an object.
                    let argc = self.bytecode[pc + 1] as usize;
                    let callee = self.pop();
                    let mut args: Vec<Value> = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(self.pop());
                    }
                    args.reverse();
                    if let Some(f) = callee.as_native() {
                        // Native constructor (Map/Set): the native builds and
                        // returns the instance itself (it captures its
                        // prototype); there is no `this` to bind.
                        let result = f(&args, self);
                        if let Some(p) = self.native_throw_jump.take() {
                            pc = p;
                            continue;
                        }
                        if self.uncaught_exception.is_some() {
                            pc = usize::MAX;
                            continue;
                        }
                        self.push(result);
                        pc += 2;
                        continue;
                    }
                    let instance = match callee.as_function() {
                        Some(f) => {
                            let proto = f
                                .props
                                .borrow()
                                .as_ref()
                                .and_then(|p| p.borrow().get("prototype").cloned())
                                .unwrap_or(Value::undefined());
                            Value::object_with_proto(proto)
                        }
                        None => {
                            // Not a constructor.
                            match self.throw_value(Value::string(format!(
                                "TypeError: {} is not a constructor",
                                callee.type_name()
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    };
                    self.push(instance);
                    for a in args {
                        self.push(a);
                    }
                    let base_slot = self.stack.len() - argc;
                    let inst_slot = base_slot.saturating_sub(1);
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        true,
                        Some(inst_slot),
                        true,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the ctor body runs. The Return opcode (is_ctor frames)
                    // keeps the instance unless the ctor returns an object;
                    // natives clean up inside dispatch_call.
                }
                Opcode::NewSpread => {
                    // `new C(...args)`: like New, but the argument count is
                    // dynamic (the argc byte counts argument SLOTS and the
                    // mask marks the spreads, exactly like CallSpread).
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let callee = self.pop();
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let args = expand_spreads(vals, mask);
                    if let Some(f) = callee.as_native() {
                        let result = f(&args, self);
                        if let Some(p) = self.native_throw_jump.take() {
                            pc = p;
                            continue;
                        }
                        if self.uncaught_exception.is_some() {
                            pc = usize::MAX;
                            continue;
                        }
                        self.push(result);
                        pc += 4;
                        continue;
                    }
                    let instance = match callee.as_function() {
                        Some(f) => {
                            let proto = f
                                .props
                                .borrow()
                                .as_ref()
                                .and_then(|p| p.borrow().get("prototype").cloned())
                                .unwrap_or(Value::undefined());
                            Value::object_with_proto(proto)
                        }
                        None => {
                            match self.throw_value(Value::string(format!(
                                "TypeError: {} is not a constructor",
                                callee.type_name()
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    };
                    self.push(instance);
                    for a in &args {
                        self.push(a.clone());
                    }
                    let base_slot = self.stack.len() - args.len();
                    let inst_slot = base_slot.saturating_sub(1);
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        true,
                        Some(inst_slot),
                        true,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::CallSpread | Opcode::CallSpreadKeep0 => {
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let callee = self.pop();
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let args = expand_spreads(vals, mask);
                    // Re-push the materialized arguments so the shared dispatch
                    // builds the frame from the operand stack as usual.
                    for a in &args {
                        self.push(a.clone());
                    }
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        matches!(op, Opcode::CallSpread),
                        None,
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::Return => {
                    let val = self.pop();
                    if let Some(frame) = self.call_stack.pop() {
                        // A constructor that returns a non-object (or nothing)
                        // yields the fresh instance instead — JS semantics.
                        let val = if frame.is_ctor && !val.is_object_like() {
                            frame
                                .this_slot
                                .map(|s| self.stack.at(s).clone())
                                .unwrap_or(Value::undefined())
                        } else {
                            val
                        };
                        // An async function's return resolves its promise; the
                        // promise (not the raw value) goes back to the caller.
                        let to_caller = if let Some(ps) = frame.promise_slot {
                            let promise = self.stack.at(frame.base_slot + ps as usize).clone();
                            if let Some(p) = promise.as_promise() {
                                self.resolve_promise(p, val.clone());
                            }
                            promise
                        } else {
                            val
                        };
                        self.stack.truncate(frame.base_slot);
                        // Method/ctor receiver cleanup: the receiver sits below
                        // the args (this_slot < base_slot). Remove it so only
                        // the call result remains — JS functions are cleaned up
                        // here; natives are cleaned up inside dispatch_call.
                        if let Some(ts) = frame.this_slot {
                            self.stack.truncate(ts);
                        }
                        self.cells_stack.truncate(frame.cells_len);
                        self.handlers.truncate(frame.handlers_len);
                        // A restored continuation's caller already received the
                        // promise at suspension; end this dispatch instead of
                        // re-running the caller.
                        if frame.resumed && self.call_stack.is_empty() {
                            break;
                        }
                        // Statement-position calls (keep=0) don't receive the
                        // result; the callee and args are already consumed.
                        if frame.keep_result {
                            self.push(to_caller);
                        }
                        if frame.return_program != self.program_id {
                            self.load_program(frame.return_program);
                        }
                        pc = frame.return_addr;
                    } else {
                        self.push(val);
                        break;
                    }
                }

                Opcode::Throw => {
                    let exc = self.pop();
                    match self.throw_value(exc) {
                        ThrowResult::Jump(p) => pc = p,
                        ThrowResult::EndDispatch => break,
                        ThrowResult::Abort => break,
                    }
                }
                Opcode::TryStart => {
                    let handler_pc = self.read_u32(pc + 1) as usize;
                    self.handlers.push(Handler {
                        stack_depth: self.stack.len(),
                        handler_pc,
                        frame_depth: self.call_stack.len(),
                    });
                    pc += 5;
                }
                Opcode::TryEnd => {
                    self.handlers.pop();
                    pc += 1;
                }

                Opcode::NewPromise => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let promise = self.new_promise();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    self.record_local(idx);
                    while self.stack.len() <= idx {
                        self.stack.push(Value::undefined());
                    }                        *self.stack.at_mut(idx) = promise.clone();
                    self.stack.mark_kind(idx, KIND_OTHER);
                    if let Some(f) = self.call_stack.last_mut() {
                        f.promise_slot = Some(slot as u8);
                    }
                    pc += 2;
                }
                Opcode::Await => {
                    let val = self.pop();
                    if let Some(p) = val.as_promise() {
                        let status = p.lock().unwrap_or_else(|g| g.into_inner()).status.clone();
                        match status {
                            PromiseStatus::Fulfilled(v) => {
                                self.push(v);
                                pc += 1;
                            }
                            // Awaiting a rejected promise throws the
                            // rejection reason, like JS.
                            PromiseStatus::Rejected(v) => {
                                match self.throw_value(v) {
                                    ThrowResult::Jump(p) => pc = p,
                                    ThrowResult::EndDispatch => break,
                                    ThrowResult::Abort => break,
                                }
                            }
                            PromiseStatus::Pending => {
                                    // Find the innermost async invocation and
                                    // suspend it, returning its promise to the
                                    // caller.
                                    let boundary = match self
                                        .call_stack
                                        .iter()
                                        .rposition(|f| f.promise_slot.is_some())
                                    {
                                        Some(i) => i,
                                        None => {
                                            // Defensive: no async frame.
                                            self.push(Value::undefined());
                                            pc += 1;
                                            continue;
                                        }
                                    };
                                    let b = self.call_stack[boundary].clone();
                                    let id = self.next_cont_id;
                                    self.next_cont_id += 1;
                                    self.call_stack[boundary].resumed = true;
                                    // The saved stack starts at the boundary
                                    // frame's base, so rebase the saved frames'
                                    // slots to match (the caller's region below
                                    // is not part of this continuation).
                                    let mut frames: Vec<CallFrame> =
                                        self.call_stack[boundary..].to_vec();
                                    for f in frames.iter_mut() {
                                        f.base_slot -= b.base_slot;
                                        f.cells_len -= b.cells_len;
                                        f.handlers_len -= b.handlers_len;
                                        f.locals_end -= b.base_slot;
                                    }
                                    // The saved frames' exception handlers move
                                    // with the continuation; the caller's stay
                                    // active.
                                    let saved_handlers =
                                        self.handlers[b.handlers_len..].to_vec();
                                    self.handlers.truncate(b.handlers_len);
                                    self.continuations.insert(
                                        id,
                                        Continuation::Suspended {
                                            stack: self.stack.save_from(b.base_slot),
                                            frames,
                                            cells: self.cells_stack[b.cells_len..].to_vec(),
                                            handlers: saved_handlers,
                                            pc: pc + 1,
                                            program_id: self.program_id,
                                        },
                                    );
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .continuations
                                        .push(id);
                                    // Return the async invocation's own promise
                                    // to its caller (skipped for keep=0
                                    // statement-position calls, which discard
                                    // it — the continuation still runs).
                                    let own = self.stack.at(b.base_slot + b.promise_slot.unwrap() as usize).clone();
                                    self.stack.truncate(b.base_slot);
                                    self.call_stack.truncate(boundary);
                                    self.cells_stack.truncate(b.cells_len);
                                    if b.keep_result {
                                        self.push(own);
                                    }
                                    pc = b.return_addr;
                                }
                            }
                        } else {
                            // Await on a non-promise: pass through.
                            self.push(val);
                            pc += 1;
                        }
                    }

                Opcode::MakeArray => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mut elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        elements.push(self.pop());
                    }
                    elements.reverse();
                    self.push(Value::array(elements));
                    pc += 3;
                }
                Opcode::MakeArraySpread => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mask = self.read_u16(pc + 3);
                    let mut vals: Vec<Value> = (0..count).map(|_| self.pop()).collect();
                    vals.reverse();
                    let elements = expand_spreads(vals, mask);
                    self.push(Value::array(elements));
                    pc += 5;
                }
                Opcode::MakeRestArray => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let fixed = self.bytecode[pc + 2] as usize;
                    // At function entry the stack top is exactly base + argc,
                    // so everything past the fixed params is the rest.
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let argc = self.stack.len().saturating_sub(base);
                    let mut items = Vec::new();
                    for i in (base + fixed)..(base + argc) {
                        items.push(self.stack.at(i).clone());
                    }
                    let idx = base + slot;
                    self.record_local(idx);
                    while self.stack.len() <= idx {
                        self.stack.push(Value::undefined());
                    }
                    *self.stack.at_mut(idx) = Value::array(items);
                    self.stack.mark_kind(idx, KIND_OTHER);
                    pc += 3;
                }
                Opcode::ArraySlice => {
                    let start = self.bytecode[pc + 1] as usize;
                    let obj = self.pop();
                    let items: Vec<Value> = if let Some(arr) = obj.as_array() {
                        let arr = arr.borrow();
                        arr.to_values().into_iter().skip(start).collect()
                    } else if let Some(s) = obj.as_str() {
                        s.chars()
                            .skip(start)
                            .map(|c| Value::string(c.to_string()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    self.push(Value::array(items));
                    pc += 2;
                }
                Opcode::MakeObject => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mask = self.read_u16(pc + 3);
                    // Fields are pushed in source order but popped LIFO, so
                    // collect then reverse — object shapes keep JS insertion
                    // order (JSON.stringify and ordered iteration rely on it).
                    // A set mask bit marks a spread: the stack holds ONE value
                    // (the source), whose own enumerable properties are
                    // expanded in place; `{...null}`/`{...undefined}` add
                    // nothing, like JS.
                    let mut pairs: Vec<(String, Value)> = Vec::with_capacity(count + 8);
                    for i in (0..count).rev() {
                        if mask & (1 << i) != 0 {
                            let src = self.pop();
                            if let Some(mut ps) = object_spread_pairs(&src) {
                                // Fields are popped in reverse source order
                                // and the whole list is reversed below, so a
                                // spread's own entries must go in reversed
                                // order here to end up in source order.
                                pairs.extend(ps.drain(..).rev());
                            }
                        } else {
                            let val = self.pop();
                            // Keys are coerced with ToString (JS spec):
                            // computed keys may be numbers/booleans/etc.
                            let key = to_string_js(&self.pop());
                            pairs.push((key, val));
                        }
                    }
                    pairs.reverse();
                    self.push(Value::object_ordered(pairs));
                    pc += 5;
                }
                Opcode::GetProperty => {
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    self.push(val);
                    pc += 3;
                }
                Opcode::GetPropertyCell => {
                    // Live-import binds: return the RAW property value (the
                    // cell itself) so StoreGlobal below aliases the module's
                    // own storage. Reads of the bound name then go through
                    // LoadGlobal's cell unwrap and always see the current
                    // value — ESM live-binding semantics.
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let val = self.get_prop_cell_value(&obj, &prop);
                    self.push(val);
                    pc += 3;
                }
                Opcode::LoadLocalGetPropConst => {
                    // `local.prop` (const prop): one dispatch instead of
                    // LoadLocal + LoadConst + GetProperty. The hottest case
                    // is `arr.length` in loop conditions; objects take the
                    // same monomorphic-IC path as GetProperty.
                    let slot = self.bytecode[pc + 1] as usize;
                    let idx = self.read_u16(pc + 2);
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + slot < self.stack.len() {
                        self.slot_value(base + slot)
                    } else {
                        Value::undefined()
                    };
                    let prop = self.constants[idx as usize].clone();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    self.push(val);
                    pc += 4;
                }
                Opcode::LoadLocalLocalGetIndex => {
                    // `a[i]` with both operands locals: one dispatch instead
                    // of LoadLocal + LoadLocal + GetIndex — the packed-int
                    // array path stays in a single hot opcode.
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&obj, &idx);
                    self.push(val);
                    pc += 3;
                }
                Opcode::CompoundPropConst => {
                    // [obj] -> (obj.p = obj.p ar imm): one dispatch for
                    // `o.a += 1`. The RHS is a compile-time constant, so the
                    // JS read-old-before-RHS order is unobservably preserved.
                    // ar bits 0-3 = arith code (0-10), bit 4 = keep result
                    // (0 in statement context).
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let idx = self.read_u16(pc + 2);
                    let imm = self.read_i32(pc + 4) as i64;
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let old = self.get_prop_value(pc, &obj, &prop);
                    let new = arith_apply(&old, &Value::int(imm), ar);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 8;
                }
                Opcode::PeekProperty => {
                    // [obj] -> [obj, obj.p]: read without popping, so the RHS
                    // can evaluate before the write (JS order) and the obj
                    // evaluates exactly once.
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.peek();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    self.push(val);
                    pc += 3;
                }
                Opcode::ArithWriteProp => {
                    // [obj, old, rhs] -> (obj.p = old ar rhs); ar bits 0-3 =
                    // arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let idx = self.read_u16(pc + 2);
                    let prop = self.constants[idx as usize].clone();
                    let rhs = self.pop();
                    let old = self.pop();
                    let obj = self.pop();
                    let new = arith_apply(&old, &rhs, ar);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 4;
                }
                Opcode::SetProperty => {
                    let prop = self.pop();
                    let obj = self.pop();
                    let val = self.pop();
                    self.set_prop_value(pc, &obj, &prop, val);
                    pc += 1;
                }
                Opcode::SetAccessor => {
                    // Class getter/setter install: pops [name, obj, fn] and
                    // stores fn as the getter (kind 1) or setter (kind 2) of
                    // obj[name]. Plain SetProperty can't do this — accessors
                    // live in ObjectData.accessors, never the shape (see
                    // get_prop/set_prop).
                    let kind = self.bytecode[pc + 1];
                    let name = self.pop();
                    let obj = self.pop();
                    let f = self.pop();
                    if let Some(m) = obj.as_object() {
                        if let Some(n) = name.as_str() {
                            let mut m = m.borrow_mut();
                            let accs = m.accessors.get_or_insert_with(Default::default);
                            let e = accs
                                .entry(n.to_string())
                                .or_insert((Value::undefined(), Value::undefined()));
                            if kind == 1 {
                                e.0 = f;
                            } else {
                                e.1 = f;
                            }
                        }
                    }
                    pc += 2;
                }
                Opcode::IncPropConst => {
                    // [obj] -> (prefix ? new : old), new = obj.p ± 1 written
                    // back (pushed only if keep; flags bit 2). Inc/dec has no
                    // RHS, so one dispatch is fully spec-correct: obj
                    // evaluated, old read, write.
                    let flags = self.bytecode[pc + 1];
                    let keep = flags & 4 != 0;
                    let idx = self.read_u16(pc + 2);
                    let prop = self.constants[idx as usize].clone();
                    let delta = if flags & 2 != 0 { -1 } else { 1 };
                    let prefix = flags & 1 != 0;
                    let obj = self.pop();
                    let old = self.get_prop_value(pc, &obj, &prop);
                    let new = arith_apply(&old, &Value::int(delta), 0);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(if prefix { new } else { old });
                    }
                    pc += 4;
                }
                Opcode::IncIndexConst => {
                    // [obj, idx] -> (prefix ? new : old), new = obj[idx] ± 1
                    // written back (pushed only if keep; flags bit 2). The obj
                    // and index each evaluate exactly once and stay on the
                    // stack.
                    let flags = self.bytecode[pc + 1];
                    let keep = flags & 4 != 0;
                    let delta = if flags & 2 != 0 { -1 } else { 1 };
                    let prefix = flags & 1 != 0;
                    let idx = self.pop();
                    let obj = self.pop();
                    let old = self.get_index_value(&obj, &idx);
                    let new = arith_apply(&old, &Value::int(delta), 0);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(if prefix { new } else { old });
                    }
                    pc += 2;
                }
                Opcode::GetIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let val = self.get_index_value(&obj, &idx);
                    self.push(val);
                    pc += 1;
                }
                Opcode::CompoundIndexConst => {
                    // [obj, idx] -> (obj[idx] = obj[idx] ar imm): one dispatch
                    // for `keyed[key] += 1`. The RHS is a compile-time
                    // constant, so the read-old-before-RHS order is preserved.
                    // ar bits 0-3 = arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let imm = self.read_i32(pc + 2) as i64;
                    let idx = self.pop();
                    let obj = self.pop();
                    let old = self.get_index_value(&obj, &idx);
                    let new = arith_apply(&old, &Value::int(imm), ar);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 6;
                }
                Opcode::PeekIndex => {
                    // [obj, idx] -> [obj, idx, obj[idx]]: read without popping,
                    // so the RHS evaluates before the write (JS order).
                    let idx = self.peek();
                    let obj = self.stack.at(self.stack.len() - 2).clone();
                    let val = self.get_index_value(&obj, &idx);
                    self.push(val);
                    pc += 1;
                }
                Opcode::ArithWriteIndex => {
                    // [obj, idx, old, rhs] -> (obj[idx] = old ar rhs); ar bits
                    // 0-3 = arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let rhs = self.pop();
                    let old = self.pop();
                    let idx = self.pop();
                    let obj = self.pop();
                    let new = arith_apply(&old, &rhs, ar);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 2;
                }
                Opcode::GetKeys => {
                    let obj = self.pop();
                    let keys: Vec<Value> = match obj.as_object() {
                        Some(m) => {
                            let m = m.borrow();
                            // Deterministic order (hash maps are unordered).
                            m.keys_live()
                                .into_iter()
                                .map(|k| Value::string(k.clone()))
                                .collect()
                        }
                        None => Vec::new(),
                    };
                    self.push(Value::array(keys));
                    pc += 1;
                }
                Opcode::SetIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let val = self.pop();
                    self.set_index_value(&obj, &idx, val);
                    pc += 1;
                }

                // ---- condition fusions (compare/jump in one dispatch) ----
                Opcode::CmpLocalIntJumpIfFalsePop => {
                    // r{slot} cmp imm, jump on falsy: `while (n !== 1)`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp = self.bytecode[pc + 6];
                    let target = self.read_u32(pc + 7) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = if idx < self.stack.len() {
                        match self.stack.kind_of(idx) {
                            KIND_INT => cmp_i64(
                                Value::int_bits_raw(self.stack.at(idx).bits()),
                                imm,
                                cmp,
                            ),
                            KIND_NUMBER => cmp_f64(
                                f64::from_bits(self.stack.at(idx).bits()),
                                imm as f64,
                                cmp,
                            ),
                            _ => compare_values(&self.slot_value(idx), &Value::int(imm), cmp),
                        }
                    } else {
                        compare_values(&Value::undefined(), &Value::int(imm), cmp)
                    };
                    if result {
                        pc += 11;
                    } else {
                        pc = target;
                    }
                }
                Opcode::CmpLocalLocalJumpIfFalsePop => {
                    // r{a} cmp r{b}, jump on falsy: `for (j = lo; j < hi; …)`.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp = self.bytecode[pc + 3];
                    let target = self.read_u32(pc + 4) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let ia = base + a;
                    let ib = base + b;
                    let result = if ia < self.stack.len() && ib < self.stack.len() {
                        match (self.stack.kind_of(ia), self.stack.kind_of(ib)) {
                            (KIND_INT, KIND_INT) => cmp_i64(
                                Value::int_bits_raw(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_INT, KIND_NUMBER) => cmp_f64(
                                Value::int_bits_raw(self.stack.at(ia).bits()) as f64,
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_INT) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()) as f64,
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_NUMBER) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            _ => compare_values(&self.slot_value(ia), &self.slot_value(ib), cmp),
                        }
                    } else {
                        let va = if ia < self.stack.len() { self.slot_value(ia) } else { Value::undefined() };
                        let vb = if ib < self.stack.len() { self.slot_value(ib) } else { Value::undefined() };
                        compare_values(&va, &vb, cmp)
                    };
                    if result {
                        pc += 8;
                    } else {
                        pc = target;
                    }
                }
                Opcode::LoadIndexCmpLocalJumpIfFalsePop => {
                    // a[objs][idxs] cmp kslot, jump on falsy: `a[j] > key`.
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let k_slot = self.bytecode[pc + 3] as usize;
                    let cmp = self.bytecode[pc + 4];
                    let target = self.read_u32(pc + 5) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let v = self.get_index_value(&obj, &idx);
                    let k = if base + k_slot < self.stack.len() {
                        self.slot_value(base + k_slot)
                    } else {
                        Value::undefined()
                    };
                    let result = compare_values(&v, &k, cmp_semantic(
                        Opcode::from_u8(cmp).unwrap_or(Opcode::StrictEqual),
                    ));
                    if result {
                        pc += 9;
                    } else {
                        pc = target;
                    }
                }
                Opcode::ArithLocalIntCmpJumpIfFalsePop => {
                    // (r{slot} ar imm1) cmp imm2, jump on falsy:
                    // `n % 2 === 0`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm1 = self.read_i32(pc + 2) as i64;
                    let ar = self.bytecode[pc + 6];
                    let imm2 = self.read_i32(pc + 7) as i64;
                    let cmp = self.bytecode[pc + 11];
                    let target = self.read_u32(pc + 12) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (res, fast) = alu_local_imm(&self.stack, idx, imm1, ar);
                    let cmp = cmp_semantic(Opcode::from_u8(cmp).unwrap_or(Opcode::StrictEqual));
                    let result = if fast {
                        compare_values(&res, &Value::int(imm2), cmp)
                    } else {
                        let l = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        let v = arith_apply(&l, &Value::int(imm1), ar);
                        compare_values(&v, &Value::int(imm2), cmp)
                    };
                    if result {
                        pc += 16;
                    } else {
                        pc = target;
                    }
                }

                // ---- index-write fusions (swap/shift shapes) ----
                Opcode::SetIndexLocalLocal => {
                    // arr[objs][idxs] = vslot (`arr[i] = tmp`).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let v_slot = self.bytecode[pc + 3] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = if base + v_slot < self.stack.len() {
                        self.slot_value(base + v_slot)
                    } else {
                        Value::undefined()
                    };
                    self.set_index_value(&obj, &idx, val);
                    pc += 4;
                }
                Opcode::SetIndexLocalGetLocal => {
                    // arr[objs][idxs] = brr[vobjs][vidxs]
                    // (`arr[i] = arr[j]` swap write).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let v_obj_slot = self.bytecode[pc + 3] as usize;
                    let v_idx_slot = self.bytecode[pc + 4] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let vobj = if base + v_obj_slot < self.stack.len() {
                        self.slot_value(base + v_obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let vidx = if base + v_idx_slot < self.stack.len() {
                        self.slot_value(base + v_idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&vobj, &vidx);
                    self.set_index_value(&obj, &idx, val);
                    pc += 5;
                }
                Opcode::SetIndexLocalPlusIntLocalGetLocal => {
                    // arr[objs][idxs + imm] = brr[vobjs][vidxs]
                    // (`a[j + 1] = a[j]` shift).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let ar = self.bytecode[pc + 3];
                    let imm = self.read_i32(pc + 4) as i64;
                    let v_obj_slot = self.bytecode[pc + 8] as usize;
                    let v_idx_slot = self.bytecode[pc + 9] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let widx = if base + v_idx_slot < self.stack.len() {
                        self.slot_value(base + v_idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let wobj = if base + v_obj_slot < self.stack.len() {
                        self.slot_value(base + v_obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&wobj, &widx);
                    let (new_idx, fast) = alu_local_imm(&self.stack, base + idx_slot, imm, ar);
                    let new_idx = if fast {
                        new_idx
                    } else {
                        let l = if base + idx_slot < self.stack.len() {
                            self.slot_value(base + idx_slot)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&l, &Value::int(imm), ar)
                    };
                    self.set_index_value(&obj, &new_idx, val);
                    pc += 10;
                }

                Opcode::Pop => { self.pop(); pc += 1; }
                Opcode::Dup => {
                    let val = self.peek();
                    self.push(val);
                    pc += 1;
                }

                Opcode::TypeOf => {
                    let val = self.pop();
                    self.push(Value::string(val.type_name().to_string()));
                    pc += 1;
                }
                Opcode::Print => {
                    let val = self.pop();
                    let s = format!("{}\n", val);
                    print!("{}", s);
                    pc += 1;
                }

                Opcode::AllocShared => {
                    let size = self.read_u16(pc + 1) as usize;
                    // Allocate from the sidecar segment (never leaked) instead
                    // of a raw heap allocation that can never be freed.
                    match self.shared.bump(size) {
                        Ok(offset) => {
                            let ptr = unsafe { self.shared.raw_ptr().add(offset) };
                            self.push(Value::buffer(ptr, size));
                        }
                        Err(e) => {
                            eprintln!("alloy shared memory error: {}", e);
                            self.push(Value::undefined());
                        }
                    }
                    pc += 3;
                }
                Opcode::ReadShared => {
                    let offset = self.read_u16(pc + 1) as usize;
                    let len = self.read_u16(pc + 3) as usize;
                    if let Ok(slice) = self.shared.read(offset, len) {
                        let s = String::from_utf8_lossy(slice).to_string();
                        self.push(Value::string(s));
                    } else {
                        self.push(Value::undefined());
                    }
                    pc += 5;
                }
                Opcode::WriteShared => {
                    pc += 5;
                }

                Opcode::Send => { pc += 1; }
                Opcode::Receive => { self.push(Value::undefined()); pc += 1; }
                Opcode::Spawn => {
                    // Pop a function and push a promise that resolves with its
                    // result: the task runs as its own isolated frame on the
                    // event loop, exactly like `spawn(fn)` the native.
                    let f = self.pop();
                    let p = self.vm_spawn_fn(&f, &[]);
                    self.push(p);
                    pc += 1;
                }

                Opcode::LoadPython => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let src = match self.constants.get(idx).and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => {
                            self.push(Value::undefined());
                            pc += 3;
                            continue;
                        }
                    };
                    let m = self.python_module(&src);
                    self.push(m);
                    pc += 3;
                }

                Opcode::CaptureLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let v = if idx < self.stack.len() {
                        self.stack.at(idx).clone()
                    } else {
                        Value::undefined()
                    };
                    if v.is_cell() {
                        self.push(v);
                    } else {
                        // Wrap the current value in a cell and write it back
                        // to the slot so the enclosing function sees later
                        // mutations through the closure. The slot and the
                        // pushed capture share the same cell.
                        let cell = Value::cell(v);
                        self.record_local(idx);
                        if idx < self.stack.len() {
                            *self.stack.at_mut(idx) = cell.clone();
                        } else {
                            while self.stack.len() <= idx {
                                self.stack.push(Value::undefined());
                            }
                            *self.stack.at_mut(idx) = cell.clone();
                        }
                        // The slot now holds a cell, not the value — the
                        // int/number fast lanes must not fire on it.
                        self.stack.mark_kind(idx, KIND_OTHER);
                        self.push(cell);
                    }
                    pc += 2;
                }
                Opcode::CaptureUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let v = self
                        .cells_stack
                        .last()
                        .and_then(|c| c.get(i))
                        .cloned()
                        .map(Value::cell_rc)
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 2;
                }
                Opcode::WrapCell => {
                    // Arrow lexical capture: freeze the pushed `this` /
                    // `arguments` value in a fresh cell so NewClosure (which
                    // pops captures assuming they are already cells) keeps
                    // it instead of substituting undefined.
                    let v = self.pop();
                    self.push(Value::cell(v));
                    pc += 1;
                }
                Opcode::NewClosure => {
                    let ci = self.read_u16(pc + 1) as usize;
                    let count = self.bytecode[pc + 3] as usize;
                    let params = self.bytecode[pc + 4] as usize;
                    let uses_args = self.bytecode[pc + 5] as usize;
                    let ptr = match self.constants.get(ci) {
                        Some(v) => v.to_number(),
                        None => 0.0,
                    };
                    let mut cells = Vec::with_capacity(count);
                    for _ in 0..count {
                        let c = self.pop();
                        if let Some(cell) = c.as_cell_rc() {
                            cells.push(cell);
                        } else {
                            cells.push(Rc::new(RefCell::new(Value::undefined())));
                        }
                    }
                    // Captures were pushed in upvalue-index order, so pop()
                    // reversed them; restore the compiler's ordering.
                    cells.reverse();
                    self.push(Value::function(FunctionData {
                        program: self.program_id,
                        ptr: ptr as usize,
                        params: params as u8,
                        uses_args: uses_args as u8,
                        cells,
                        props: RefCell::new(None),
                    }));
                    pc += 6;
                }
                Opcode::LoadUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let v = self
                        .cells_stack
                        .last()
                        .and_then(|c| c.get(i))
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 2;
                }
                Opcode::StoreUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let cell = self.cells_stack.last().and_then(|c| c.get(i)).cloned();
                    if let Some(cell) = cell {
                        self.note_rc_dirty(RcDirtyRef::Cell(cell.clone()));
                        *cell.borrow_mut() = val;
                    }
                    pc += 2;
                }
                Opcode::LoadCell => {
                    let i = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let v = if base + i < self.stack.len() {
                        self.stack.at(base + i).clone()
                    } else {
                        Value::undefined()
                    };
                    self.push(v);
                    pc += 2;
                }
                Opcode::StoreCell => {
                    let i = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + i;
                    self.record_local(idx);
                    let k = kind_of_value(&val);
                    if idx < self.stack.len() {
                        *self.stack.at_mut(idx) = val;
                    } else {
                        while self.stack.len() <= idx {
                            self.stack.push(Value::undefined());
                        }
                        *self.stack.at_mut(idx) = val;
                    }
                    self.stack.mark_kind(idx, k);
                    pc += 2;
                }
                Opcode::LoadSelf => {
                    let v = self
                        .call_stack
                        .last()
                        .map(|f| f.fn_value.clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::LoadThis => {
                    let v = self
                        .call_stack
                        .last()
                        .and_then(|f| f.this_slot)
                        .map(|s| self.stack.at(s).clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::LoadArguments => {
                    // The `arguments` object: an array snapshot of the passed
                    // args (extra args beyond the params count are included;
                    // missing params are not). An array gives `.length` and
                    // for-of/spread for free. The snapshot was taken at call
                    // entry (the body's local stores clobber the arg slots);
                    // fall back to a live stack read only for frames that
                    // never took one (legacy paths). Outside any function the
                    // compiler never emits this.
                    let v = match self.call_stack.last() {
                        Some(f) => {
                            let values: Vec<Value> = match &f.arg_values {
                                Some(v) => v.clone(),
                                None => (0..f.argc)
                                    .map(|i| self.stack.at(f.base_slot + i).clone())
                                    .collect(),
                            };
                            Value::array(values)
                        }
                        None => Value::undefined(),
                    };
                    self.push(v);
                    pc += 1;
                }
                Opcode::GetProto => {
                    let obj = self.pop();
                    let v = obj
                        .as_object()
                        .map(|od| od.borrow().proto.clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::SetProto => {
                    let proto = self.pop();
                    let obj = self.pop();
                    if let Some(od) = obj.as_object() {
                        od.borrow_mut().proto = proto;
                    }
                    pc += 1;
                }
                Opcode::InstanceOf => {
                    let ctor = self.pop();
                    let obj = self.pop();
                    let proto = match ctor.as_function() {
                        Some(f) => match f.props.borrow().as_ref() {
                            Some(p) => p
                                .borrow()
                                .get("prototype")
                                .cloned()
                                .unwrap_or(Value::undefined()),
                            None => Value::undefined(),
                        },
                        // Native constructors (Map/Set) carry their prototype.
                        None => ctor.as_native_proto().unwrap_or(Value::undefined()),
                    };
                    let mut found = false;
                    let mut cur = obj;
                    // Depth-limited walk: a proto chain can only be as long as
                    // the object graph, so 1024 is unreachable in practice.
                    for _ in 0..1024 {
                        match cur.as_object() {
                            Some(od) => {
                                let p = od.borrow().proto.clone();
                                if p.is_undefined() {
                                    break;
                                }
                                if p.bits() == proto.bits() {
                                    found = true;
                                    break;
                                }
                                cur = p;
                            }
                            None => break,
                        }
                    }
                    self.push(Value::bool(found));
                    pc += 1;
                }
                Opcode::In => {
                    // `key in obj` — checks the OWN properties AND the
                    // prototype chain (JS semantics: `"toString" in {}` is
                    // true). The key is coerced via ToString; anything that
                    // is not an object/function/array throws Node's
                    // TypeError (Map/Set are not property holders).
                    let obj = self.pop();
                    let key = self.pop();
                    let ks = to_string_js(&key);
                    match in_operator_probe(&obj, &ks) {
                        Some(found) => {
                            self.push(Value::bool(found));
                            pc += 1;
                        }
                        None => {
                            match self.throw_value(Value::string(format!(
                                "TypeError: Cannot use 'in' operator to search for '{}' in {}",
                                ks,
                                iterable_display(&obj)
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    }
                }
            }
        }

        self.pop()
    }

    /// Perform a call whose `argc` arguments (with the callee already popped)
    /// are on the operand stack. Returns the pc to continue at: a bytecode
    /// target for script/closure calls, or `ret_addr` for natives (their
    /// result is already pushed).
    fn dispatch_call(
        &mut self,
        callee: Value,
        argc: usize,
        ret_addr: usize,
        keep: bool,
        this_slot: Option<usize>,
        is_ctor: bool,
    ) -> usize {
        // Guard against runaway recursion: past the limit, calls return
        // undefined instead of growing the stack forever.
        if self.call_stack.len() >= MAX_CALL_DEPTH {
            for _ in 0..argc {
                self.pop();
            }
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            return ret_addr;
        }
        // Stack-space guard: the operand stack is a fixed array, so a frame
        // that would land near the top fails gracefully (same shape as the
        // recursion guard) instead of overflowing the stack.
        let base_slot = self.stack.len() - argc;
        if base_slot + FRAME_BUDGET > STACK_SIZE {
            for _ in 0..argc {
                self.pop();
            }
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            return ret_addr;
        }
        if let Some(f) = callee.as_function() {
            let base_slot = self.stack.len() - argc;
            // Call-site IC: remember last callee bits per Call pc (caller passes
            // ret_addr as the site). Hit skips re-probing `as_function` next time
            // via the leaf check below — the bits compare is one u64 cmp.
            let site = (ret_addr as u32) & (IC_SLOTS as u32 - 1);
            let cb = callee.bits();
            let ic_hit = self.call_ic[site as usize].callee_bits == cb;
            if !ic_hit {
                self.call_ic[site as usize] = CallIcEntry { callee_bits: cb, func_ptr: f.ptr as u64, params: f.params };
            }
            // Missing arguments read as `undefined`, never as stale stack
            // garbage from an earlier frame (`function f(x, y)` called with
            // one arg: y must be undefined). Only the missing tail is filled;
            // extra args stay put (they were pushed by the caller).
            while self.stack.len() < base_slot + f.params as usize {
                self.push(Value::undefined());
            }
            let cells_len = self.cells_stack.len();
            // Leaf fast path: 90% of hot calls (fib, ack, collatz inner) capture
            // nothing — skip the Vec push/clone entirely.
            if !f.cells.is_empty() {
                self.cells_stack.push(f.cells.clone());
            }
            // `arguments`: snapshot the passed args at entry, but only for
            // functions that reference it (the body's local stores would
            // otherwise clobber the arg slots before a lazy read).
            let arg_values = if f.uses_args != 0 {
                Some((0..argc).map(|i| self.stack.at(base_slot + i).clone()).collect())
            } else {
                None
            };
            self.call_stack.push(CallFrame {
                return_addr: ret_addr,
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                keep_result: keep,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor,
            });
            // The caller pushed the args as generic temporaries, but as the
            // callee's params they are read by CmpLocalInt/BinLocalInt/…
            // fast lanes — give them their real kinds so feedback is correct
            // from the first instruction, not the first store (fib's `n` is
            // never stored).
            self.mark_param_kinds(base_slot);
            if f.program != self.program_id {
                self.load_program(f.program);
            }
            f.ptr        } else if let Some(f) = callee.as_native() {
            let mut args: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
            args.reverse();
            // Method natives (Map/Set methods on the prototype) read their
            // instance from `this_value`: the receiver still sits at
            // `this_slot` (the receiver cleanup below runs after the call).
            // Save/restore so a re-entrant native call (a native invoking a
            // JS callback via `call_value`) sees its own receiver.
            let saved_this = self.native_this.take();
            self.native_this = this_slot.map(|ts| self.stack.at(ts).clone());
            let result = f(&args, self);
            self.native_this = saved_this;
            // A native that threw: a handler jump means throw_value already
            // unwound the stack and pushed the exception at the handler —
            // jump there and discard the result. An uncaught top-level throw
            // (usize::MAX sentinel) aborts the dispatch loop.
            if let Some(p) = self.native_throw_jump.take() {
                return p;
            }
            if self.uncaught_exception.is_some() {
                return usize::MAX;
            }
            if keep {
                self.push(result);
            }
            // Method-call receiver cleanup: natives never run the Return
            // opcode, so the receiver must be removed here (JS functions are
            // cleaned up inside Return).
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            ret_addr
        } else if let Some(fn_ptr) = callee.as_number() {
            // Legacy path: calling a raw number jumps to it as a program
            // counter (the pre-closure calling convention).

            let cells_len = self.cells_stack.len();
            self.call_stack.push(CallFrame {
                return_addr: ret_addr,
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values: None,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                keep_result: keep,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor,
            });
            fn_ptr as usize
        } else {
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            ret_addr
        }
    }

    #[inline]
    fn read_i32(&self, offset: usize) -> i32 {
        self.read_u32(offset) as i32
    }

    /// Read a local slot value, transparently dereferencing cells (the same
    /// behavior as LoadLocal) for the fused superinstructions. When the slot
    /// feedback says the slot holds a direct int or number, the read is a raw
    /// word copy — no `as_cell` probe, no `Value::clone` payload match, no
    /// Rc traffic.
    #[inline(always)]
    fn slot_value(&self, idx: usize) -> Value {
        match self.stack.kind_of(idx) {
            KIND_INT | KIND_NUMBER => Value::from_raw_word(self.stack.at(idx).bits()),
            _ => {
                let v = self.stack.at(idx);
                match v.as_cell() {
                    Some(c) => c.borrow().clone(),
                    None => v.clone(),
                }
            }
        }
    }

    /// Mark the kinds of a freshly-pushed argument range — the callee's
    /// params. The args were pushed as generic temporaries (push invalidates
    /// to KIND_OTHER), but as params they deserve their real kinds so the
    /// load/ALU/cmp fast lanes fire from the frame's first instruction.
    fn mark_param_kinds(&mut self, base_slot: usize) {
        let mut i = base_slot;
        while i < self.stack.len() {
            self.stack.mark_kind(i, kind_of_value(self.stack.at(i)));
            i += 1;
        }
    }

    /// Write a local slot, growing the stack and writing through cells (the
    /// same behavior as StoreLocal) for the fused superinstructions. A slot
    /// known to hold a direct int/number is written without the `as_cell_rc`
    /// probe (it cannot be a cell).
    #[inline(always)]
    fn store_slot(&mut self, idx: usize, val: Value) {
        self.record_local(idx);
        let k = kind_of_value(&val);
        if idx < self.stack.len() && matches!(self.stack.kind_of(idx), KIND_INT | KIND_NUMBER) {
            *self.stack.at_mut(idx) = val;
            self.stack.mark_kind(idx, k);
        } else if idx < self.stack.len() {
            if let Some(c) = self.stack.at(idx).as_cell_rc() {
                self.note_rc_dirty(RcDirtyRef::Cell(c.clone()));
                *c.borrow_mut() = val;
                // The slot itself still holds the cell.
                self.stack.mark_kind(idx, KIND_OTHER);
            } else {
                *self.stack.at_mut(idx) = val;
                self.stack.mark_kind(idx, k);
            }
        } else {
            while self.stack.len() <= idx {
                self.stack.push(Value::undefined());
            }
            *self.stack.at_mut(idx) = val;
            self.stack.mark_kind(idx, k);
        }
    }

    /// 2-way polymorphic inline-cache property get. Primary hit is the old
    /// monomorphic fast path; secondary hit covers 2-shape sites without
    /// thrashing. 3+ shapes use the slow map lookup (megamorphic).
    #[inline]
    fn get_prop(
        &mut self,
        pc: usize,
        od: &RefCell<ObjectData>,
        prop: &Value,
        receiver: &Value,
    ) -> Value {
        let slot = pc & (IC_SLOTS - 1);
        let poly = self.ic[slot];
        let pb = prop.bits();
        // Primary probe (predictable branch: monomorphic sites always hit here).
        if poly.primary.program == self.program_id && poly.primary.pc == pc as u32 && poly.primary.prop == pb {
            let od = od.borrow();
            if od.shape_ptr() == poly.primary.shape && (poly.primary.offset as usize) < od.values.len() {
                return unwrap_cell(od.values[poly.primary.offset as usize].clone());
            }
        } else if poly.secondary.program == self.program_id && poly.secondary.pc == pc as u32 && poly.secondary.prop == pb {
            let od = od.borrow();
            if od.shape_ptr() == poly.secondary.shape && (poly.secondary.offset as usize) < od.values.len() {
                return unwrap_cell(od.values[poly.secondary.offset as usize].clone());
            }
        }
        let name = match prop.as_str() {
            Some(s) => s,
            None => return Value::undefined(),
        };
        // Own property first, then the prototype chain. Inherited hits are
        // NOT cached (their offset is per-ancestor, not per-receiver), so a
        // `p.dist` on a class instance always walks; the shape-IC stays for
        // the own-property fast path.
        let od = od.borrow();
        match od.shape.get(name) {
            // A deleted property reads as undefined and is not cached (it may
            // be re-set later, which clears the tombstone).
            Some(off) if !od.deleted[off as usize] => {
                let v = unwrap_cell(od.values[off as usize].clone());
                let shape = od.shape_ptr();
                drop(od);
                let fresh = IcEntry {
                    program: self.program_id,
                    pc: pc as u32,
                    shape,
                    offset: off,
                    prop: prop.bits(),
                };
                // Promote to primary, demote old primary to secondary (2-way LRU).
                let poly = &mut self.ic[slot];
                if poly.primary.shape != shape || poly.primary.prop != prop.bits() {
                    poly.secondary = poly.primary;
                    poly.primary = fresh;
                }
                return v;
            }
            _ => {}
        }
        // Own accessor: a getter is invoked with the receiver as `this`
        // (class getters on the instance's own accessor table). An accessor
        // with no callable getter reads as undefined — it does NOT fall
        // through to the prototype chain.
        if let Some((g, _)) = od
            .accessors
            .as_ref()
            .and_then(|accs| accs.get(name))
        {
            let (g, receiver) = (g.clone(), receiver.clone());
            drop(od);
            if g.is_function() || g.is_native() {
                return self.call_value_with_this(&g, Some(receiver), &[]);
            }
            return Value::undefined();
        }
        // Chain walk with owned values (the ancestor borrow cannot outlive
        // the loop iteration).
        let mut cur = od.proto.clone();
        drop(od);
        let mut depth = 0u16;
        while depth <= 1024 {
            let Some(cd) = cur.as_object() else { break };
            let cd = cd.borrow();
            match cd.shape.get(name) {
                Some(off) if !cd.deleted[off as usize] => {
                    return unwrap_cell(cd.values[off as usize].clone());
                }
                _ => {}
            }
            // Inherited accessor (a getter on a prototype): the receiver is
            // still the original object, so `this` binds correctly.
            if let Some(accs) = cd.accessors.as_ref() {
                if let Some((g, _)) = accs.get(name) {
                    if g.is_function() || g.is_native() {
                        let g = g.clone();
                        let receiver = receiver.clone();
                        drop(cd);
                        return self.call_value_with_this(&g, Some(receiver), &[]);
                    }
                }
            }
            let next = cd.proto.clone();
            drop(cd);
            cur = next;
            depth += 1;
        }
        Value::undefined()
    }

    /// Full GetProperty semantics for `obj[prop]` (objects go through the
    /// inline cache; arrays/strings/buffers/promises have their special reads;
    /// anything else is undefined). Shared by GetProperty, PeekProperty and
    /// CompoundPropConst.
    #[inline]
    /// Read a property WITHOUT unwrapping live-import cells (the raw value:
    /// the cell itself). Only object properties can be cells (module exports
    /// objects); anything else reads as undefined.
    fn get_prop_cell_value(&self, obj: &Value, prop: &Value) -> Value {
        let (od, name) = match (obj.as_object(), prop.as_str()) {
            (Some(od), Some(s)) => (od, s),
            _ => return Value::undefined(),
        };
        let od = od.borrow();
        match od.shape.get(name) {
            Some(off) if !od.deleted[off as usize] => od.values[off as usize].clone(),
            _ => Value::undefined(),
        }
    }

    fn get_prop_value(&mut self, pc: usize, obj: &Value, prop: &Value) -> Value {
        if let Some(od) = obj.as_object() {
            // Map/Set: only `size` is computed per read (it cannot be a
            // shared prototype native without getter support); the methods
            // live on Map.prototype/Set.prototype and resolve through the
            // normal proto-chain walk below.
            let container = od.borrow().container;
            if container != 0 {
                if let Some(v) = container_prop(obj, prop) {
                    return v;
                }
            }
            self.get_prop(pc, od, prop, obj)
        } else if let (Some(f), Some(s)) = (obj.as_function(), prop.as_str()) {
            // `f.call(thisArg, ...args)` / `f.apply(thisArg, args)`: invoke
            // the function with an explicit `this`. Synthesized per read like
            // the Promise `then` native.
            if s == "call" || s == "apply" {
                let is_apply = s == "apply";
                let callee = obj.clone();
                return Value::native(Arc::new(move |args, vm| {
                    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let rest: Vec<Value> = if is_apply {
                        match args.get(1) {
                            Some(a) if a.is_array() => {
                                a.as_array().map(|ad| ad.borrow().to_values()).unwrap_or_default()
                            }
                            // Node: non-array -> TypeError; the engine's
                            // non-throwing style coerces to no args.
                            _ => Vec::new(),
                        }
                    } else {
                        args.iter().skip(1).cloned().collect()
                    };
                    vm.call_value_with_this(&callee, Some(this_arg), &rest)
                }));
            }
            // Class/static properties on the function itself: `prototype`,
            // static methods. Ordinary functions have no props -> undefined.
            f.props
                .borrow()
                .as_ref()
                .and_then(|p| p.borrow().get(s).cloned())
                .unwrap_or(Value::undefined())
        } else if let (Some(props), Some(s)) = (obj.as_native_props(), prop.as_str()) {
            // Native statics (`String.fromCharCode`) and the constructor's
            // `prototype` property.
            if let Some(p) = props.borrow().as_ref() {
                if let Some(v) = p.borrow().get(s).cloned() {
                    return v;
                }
            }
            if s == "prototype" {
                obj.as_native_proto().unwrap_or(Value::undefined())
            } else {
                Value::undefined()
            }
        } else if let (Some(arr), Some(n)) = (obj.as_array(), prop.as_number()) {
            let arr = arr.borrow();
            let i = n as usize;
            if i < arr.len() { arr.get(i) } else { Value::undefined() }
        } else if let (Some(_), Some(s)) = (obj.as_array(), prop.as_str()) {
            array_prop(obj, s)
        } else if let (Some(_), Some(s)) = (obj.as_regex(), prop.as_str()) {
            regex_prop(obj, s)
        } else if let (Some(_), Some(prop)) = (obj.as_str(), prop.as_str()) {
            string_prop(obj, prop)
        } else if (obj.is_number() || obj.is_int()) && prop.as_str().is_some() {
            number_prop(obj, prop.as_str().unwrap())
        } else if let (Some((ptr, len)), Some(s)) = (obj.as_buffer(), prop.as_str()) {
            if s == "ptr" {
                Value::number(ptr as usize as f64)
            } else if s == "length" {
                Value::int(len as i64)
            } else {
                Value::undefined()
            }
        } else if let (Some(p), Some(s)) = (obj.as_promise(), prop.as_str()) {
            if s == "then" {
                let p2 = p.clone();
                Value::native(Arc::new(move |args, vm| {
                    let cb = args.first().cloned().unwrap_or(Value::undefined());
                    let on_rejected = args.get(1).cloned();
                    vm.then(&Value::promise(p2.clone()), cb, on_rejected)
                }))
            } else {
                Value::undefined()
            }
        } else if let (Some(st), Some(s)) = (obj.as_channel(), prop.as_str()) {
            match s {
                "send" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |args, vm| {
                        let msg = args.first().cloned().unwrap_or(Value::undefined());
                        // Incremental-GC barrier: the queue now holds a value
                        // the mark may not have seen.
                        vm.note_gc_dirty(RcDirtyRef::Channel(st2.clone()));
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        // Named channels are shared across VMs, so their
                        // messages travel as bytes — a Value is an arena
                        // pointer valid only on the sending thread. Anonymous
                        // channels are per-VM and pass raw values.
                        let item = if guard.named {
                            let mut bytes = Vec::new();
                            // Data crosses, code and state do not:
                            // functions/natives/promises inside the message
                            // coerce to undefined, like spawn.
                            write_spawn_value(&mut bytes, &msg, false, 0);
                            ChannelItem::Bytes(bytes)
                        } else {
                            ChannelItem::Raw(msg)
                        };
                        match guard.send_item(item) {
                            Some((waiter, ChannelItem::Bytes(bytes))) => {
                                drop(guard);
                                // Cross-thread routing: the waiter's promise
                                // belongs to the VM that parked it. If that's
                                // not this loop, hand the bytes to its owner
                                // (it decodes into its own heap and wakes);
                                // otherwise decode here and resolve locally.
                                let owner = waiter.as_promise().and_then(|p| {
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .owner
                                        .clone()
                                });
                                let mine = vm.wake_handle();
                                let mine_here = match (&owner, &mine) {
                                    (Some(o), Some(m)) => Arc::ptr_eq(o, m),
                                    _ => true,
                                };
                                if mine_here {
                                    let mut pos = 0;
                                    let value = decode_spawn_value(&bytes, &mut pos);
                                    vm.resolve_promise(&waiter, value);
                                } else if let Some(w) = waiter.as_promise() {
                                    owner.as_ref().unwrap().deliver(w.clone(), bytes);
                                }
                            }
                            Some((waiter, ChannelItem::Raw(msg))) => {
                                drop(guard);
                                // Anonymous (same-VM) channel: resolve the
                                // waiter directly. Defensive: if the waiter
                                // somehow belongs to another loop, serialize
                                // and route (raw values cannot cross heaps).
                                let owner = waiter.as_promise().and_then(|p| {
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .owner
                                        .clone()
                                });
                                let mine = vm.wake_handle();
                                let mine_here = match (&owner, &mine) {
                                    (Some(o), Some(m)) => Arc::ptr_eq(o, m),
                                    _ => true,
                                };
                                if mine_here {
                                    vm.resolve_promise(&waiter, msg);
                                } else {
                                    let mut bytes = Vec::new();
                                    write_spawn_value(&mut bytes, &msg, false, 0);
                                    if let Some(w) = waiter.as_promise() {
                                        owner.as_ref().unwrap().deliver(w.clone(), bytes);
                                    }
                                }
                            }
                            None => {}
                        }
                        Value::undefined()
                    }))
                }
                "recv" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |_args, vm| {
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        match guard.recv_item() {
                            Some(ChannelItem::Raw(v)) => v,
                            Some(ChannelItem::Bytes(bytes)) => {
                                // Named channel: decode into this heap.
                                let mut pos = 0;
                                decode_spawn_value(&bytes, &mut pos)
                            }
                            // Empty: park on a promise the event loop resolves
                            // when the next `send` lands. The waiter is
                            // stamped with this VM's wake handle (a send from
                            // another thread routes back and wakes us) and
                            // recorded so the event loop keeps pumping until
                            // it settles.
                            None => {
                                let p = vm.new_promise();
                                vm.park_cross_waiter(&p);
                                guard.push_waiter(p.clone());
                                p
                            }
                        }
                    }))
                }
                "tryRecv" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |_args, _vm| {
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        match guard.recv_item() {
                            Some(ChannelItem::Raw(v)) => v,
                            Some(ChannelItem::Bytes(bytes)) => {
                                let mut pos = 0;
                                decode_spawn_value(&bytes, &mut pos)
                            }
                            None => Value::undefined(),
                        }
                    }))
                }
                "len" => Value::int(st.lock().unwrap_or_else(|g| g.into_inner()).len() as i64),
                _ => Value::undefined(),
            }
        } else {
            Value::undefined()
        }
    }

    /// Incremental-GC write barrier for arena boxes: set the slot's dirty bit
    /// in the old generation's bitmap so the next unit boundary's scan
    /// re-traces the box. Young boxes are skipped (they are swept wholesale
    /// at the boundary anyway). One bitmap cell read-modify-write — no header
    /// traffic on the payload's cache line.
    #[inline]
    fn note_box_dirty(&self, box_ptr: usize) {
        self.heap.note_box_dirty(box_ptr);
    }

    /// Incremental-GC write barrier for Rc-backed structures (closure cells,
    /// promises, channels): record them so the next mark slice re-traces
    /// their (possibly new) contents. Keeps a strong ref so the structure
    /// can't dangle before the re-trace. Active only while a mark runs.
    #[inline]
    fn note_rc_dirty(&mut self, d: RcDirtyRef) {
        if let Some(m) = &mut self.mark {
            m.dirty_rc.push(d);
        }
    }

    /// SetProperty semantics for `obj[prop] = val`: only plain objects store
    /// (arrays/strings/etc. ignore writes), via the inline cache.
    #[inline]
    fn set_prop_value(&mut self, pc: usize, obj: &Value, prop: &Value, val: Value) {
        if let Some(r) = obj.as_regex() {
            // RegExp.lastIndex is the one writable regex property: sets the
            // /g /y cursor (ToLength: negatives and NaN clamp to 0).
            if prop.as_str() == Some("lastIndex") {
                let n = val.to_number();
                let len = if n.is_nan() || n.is_infinite() || n <= 0.0 {
                    0.0
                } else {
                    n.trunc()
                };
                let mut g = r.lock().unwrap_or_else(|g| g.into_inner());
                g.last_index = len as usize;
            }
        } else if let Some(od) = obj.as_object() {
            self.set_prop(pc, od, prop, val, obj);
        } else if let (Some(f), Some(s)) = (obj.as_function(), prop.as_str()) {
            // Class construction and static assignment: `C.prototype = X`,
            // `C.sm = fn`. Lazy-allocate the props map on first write via
            // the outer RefCell (the function lives behind a shared Rc).
            let mut slot = f.props.borrow_mut();
            let map = slot
                .get_or_insert_with(|| Rc::new(RefCell::new(hashbrown::HashMap::new())));
            map.borrow_mut().insert(s.to_string(), val);
        } else if let (Some(props), Some(s)) = (obj.as_native_props(), prop.as_str()) {
            // Same for natives with statics (`String.x = ...`).
            let mut slot = props.borrow_mut();
            let map = slot
                .get_or_insert_with(|| Rc::new(RefCell::new(hashbrown::HashMap::new())));
            map.borrow_mut().insert(s.to_string(), val);
        }
    }

    /// Full GetIndex semantics for `obj[idx]` (arrays index numerically,
    /// strings index by char with the O(1) ASCII fast path, objects look up
    /// the stringified key; anything else is undefined). Shared by GetIndex,
    /// PeekIndex and CompoundIndexConst.
    #[inline]
    fn get_index_value(&self, obj: &Value, idx: &Value) -> Value {
        let i = idx.to_number();
        if let Some(arr) = obj.as_array() {
            if i.is_finite() && i >= 0.0 {
                let arr = arr.borrow();
                let ix = i as usize;
                if ix < arr.len() { arr.get(ix) } else { Value::undefined() }
            } else {
                Value::undefined()
            }
        } else if let Some(s) = obj.as_str() {
            if i.is_finite() && i >= 0.0 {
                let ix = i as usize;
                // O(1) fast path for ASCII: a byte < 128 is always a char
                // boundary, so direct byte indexing is exact. Multi-byte
                // strings fall back to the char walk.
                let b = s.as_bytes();
                if ix < b.len() && b[ix] < 128 {
                    Value::char_str(b[ix])
                } else {
                    match s.chars().nth(ix) {
                        Some(c) => Value::char_str_utf8(c),
                        None => Value::undefined(),
                    }
                }
            } else {
                Value::undefined()
            }
        } else if let Some(m) = obj.as_object() {
            let m = m.borrow();
            let v = match idx.as_str() {
                // Borrow the key from the index Value — no per-access alloc.
                Some(key) => m.get(key).cloned().unwrap_or(Value::undefined()),
                None => {
                    let key = format!("{}", idx);
                    m.get(&key).cloned().unwrap_or(Value::undefined())
                }
            };
            // `m["counter"]` reads through live-import cells like `m.counter`.
            unwrap_cell(v)
        } else {
            Value::undefined()
        }
    }

    /// Full SetIndex semantics for `obj[idx] = val`: arrays resize to fit,
    /// objects set the stringified key; everything else ignores the write.
    #[inline]
    fn set_index_value(&self, obj: &Value, idx: &Value, val: Value) {
        if let Some(arr) = obj.as_array() {
            self.note_box_dirty(arr as *const RefCell<ArrayData> as usize);
            let mut arr = arr.borrow_mut();
            let i = idx.to_number();
            if i.is_finite() && i >= 0.0 {
                let ix = i as usize;
                arr.set_extend(ix, val);
            }
        } else if let Some(m) = obj.as_object() {
            self.note_box_dirty(m as *const RefCell<ObjectData> as usize);
            let mut m = m.borrow_mut();
            match idx.as_str() {
                // Borrow the key from the index Value — no per-access alloc.
                Some(key) => { m.set(key, val); }
                None => {
                    let key = format!("{}", idx);
                    m.set(&key, val);
                }
            }
        }
    }

    /// Monomorphic inline-cache property set, mirroring `get_prop`: a hit
    /// writes straight to `values[offset]` without the shape lookup or the
    /// per-write key allocation; a miss transitions the shape if needed and
    /// repopulates the cache. An own or inherited accessor intercepts the
    /// write first (setters run with the receiver as `this`; a getter-only
    /// accessor blocks the write, matching sloppy-mode JS).
    #[inline]
    fn set_prop(
        &mut self,
        pc: usize,
        od: &RefCell<ObjectData>,
        prop: &Value,
        val: Value,
        receiver: &Value,
    ) {
        self.note_box_dirty(od as *const RefCell<ObjectData> as usize);
        let slot = pc & (IC_SLOTS - 1);
        let poly = self.ic[slot];
        let pb = prop.bits();
        if poly.primary.program == self.program_id && poly.primary.pc == pc as u32 && poly.primary.prop == pb {
            let mut od = od.borrow_mut();
            if od.shape_ptr() == poly.primary.shape && (poly.primary.offset as usize) < od.values.len() {
                od.values[poly.primary.offset as usize] = val;
                od.deleted[poly.primary.offset as usize] = false;
                return;
            }
        } else if poly.secondary.program == self.program_id && poly.secondary.pc == pc as u32 && poly.secondary.prop == pb {
            let mut od = od.borrow_mut();
            if od.shape_ptr() == poly.secondary.shape && (poly.secondary.offset as usize) < od.values.len() {
                od.values[poly.secondary.offset as usize] = val;
                od.deleted[poly.secondary.offset as usize] = false;
                return;
            }
        }
        let Some(name) = prop.as_str() else {
            // Non-string property: ignored (matches prior behavior).
            return;
        };
        // Own accessor first — it takes precedence over a shadowing write.
        let own_acc = od
            .borrow()
            .accessors
            .as_ref()
            .and_then(|accs| accs.get(name).cloned());
        if let Some((_g, s)) = own_acc {
            let (s, receiver) = (s, receiver.clone());
            if s.is_function() || s.is_native() {
                self.call_value_with_this(&s, Some(receiver), &[val]);
            }
            return;
        }
        // Inherited accessor: a setter runs; a getter-only accessor blocks
        // the write (sloppy-mode semantics — no throw).
        // Read proto from the od borrow; the borrow guard is already scoped
        // out here, so nothing needs releasing before the proto-chain loop.
        let proto_clone = od.borrow().proto.clone();
        let mut cur = proto_clone;
        while let Some(cd) = cur.as_object() {
            let guard = cd.borrow();
            let acc = guard.accessors.as_ref().and_then(|a| a.get(name).cloned());
            let next = guard.proto.clone();
            drop(guard);
            if let Some((_g, s)) = acc {
                let (s, receiver) = (s, receiver.clone());
                if s.is_function() || s.is_native() {
                    self.call_value_with_this(&s, Some(receiver), &[val]);
                }
                return;
            }
            cur = next;
        }
        // Re-borrow mutably for the plain property write.
        let mut od = od.borrow_mut();
        let off = od.set(name, val);
        let shape = od.shape_ptr();
        drop(od);
        let fresh = IcEntry {
            program: self.program_id,
            pc: pc as u32,
            shape,
            offset: off,
            prop: prop.bits(),
        };
        let poly = &mut self.ic[slot];
        if poly.primary.shape != shape || poly.primary.prop != prop.bits() {
            poly.secondary = poly.primary;
            poly.primary = fresh;
        }
    }

    #[inline]
    fn read_u16(&self, offset: usize) -> u16 {
        ((self.bytecode[offset] as u16) << 8) | (self.bytecode[offset + 1] as u16)
    }

    #[inline]
    fn read_u32(&self, offset: usize) -> u32 {
        ((self.bytecode[offset] as u32) << 24)
            | ((self.bytecode[offset + 1] as u32) << 16)
            | ((self.bytecode[offset + 2] as u32) << 8)
            | (self.bytecode[offset + 3] as u32)
    }
}

/// Drain any microtask records still queued at teardown so their `Value`s
/// release their references before the arena's chunks are deallocated (the
/// arena itself never drops slots), and shut down the python workers before
/// the shared segment (declared before them) unmaps and deletes its backing
/// file — the child must be dead and joined first, or Windows keeps the file
/// open.
impl Drop for Vm {
    fn drop(&mut self) {
        while let Some(addr) = self.microtasks.pop_front() {
            unsafe {
                drop(std::ptr::read(addr as *const Microtask));
            }
        }
        self.shutdown_python_workers();
        // Join every spawn worker: they finish as soon as their completion
        // send fails (the receiver is gone), so the joins are bounded.
        for h in self.spawn_workers.drain(..) {
            let _ = h.join();
        }
    }
}

impl Vm {
    /// Kill every python child, drop the request queues, and join the worker
    /// threads so no sidecar outlives the VM (also used by `reload` to tear
    /// down one file's pool before re-importing it).
    fn shutdown_python_workers(&mut self) {
        let mut workers: Vec<PythonWorker> = std::mem::take(&mut self.python_workers)
            .into_values()
            .collect();
        for w in &mut workers {
            shutdown_python_pool(w);
        }
    }

    /// Tear down ONE file's pool (a `.py` reload): remove it and kill its
    /// children, shutting down sidecars and joining the worker threads so
    /// the old child processes are reaped before a fresh pool spawns.
    fn shutdown_python_worker(&mut self, src: &str) {
        if let Some(mut w) = self.python_workers.remove(src) {
            shutdown_python_pool(&mut w);
        }
    }
}

/// Kill one pool's children, stop their sidecars, and join the worker
/// threads (reload path and VM teardown). Children are killed **by pid
/// first** (no mutex needed): a worker blocked in a read on a hung child
/// hits EOF and finishes its current iteration, so the joins are bounded
/// even when a call is mid-timeout. Then the sidecar handles are locked and
/// shut down, and every worker thread is joined. Dropping the pool's remaining
/// `PythonSidecar` Arcs (here and in each worker thread's clone) reaps every
/// child exactly once.
fn shutdown_python_pool(w: &mut PythonWorker) {
    // 1. Kill every child by pid — wakes any in-flight read (EOF) without
    //    needing the sidecar mutex, so a worker can't hold up the join.
    for pid in &w.pids {
        // Embed backends report pid 0 (nothing to kill in-process).
        if *pid != 0 {
            crate::python_sidecar::kill_pid_export(*pid);
        }
    }
    // 2. Now the workers' reads have drained; lock each sidecar and stop
    //    the child for real (reap the process so its pid can't be reused
    //    while the segment's backing file is deleted).
    for sc in &w.sidecars {
        if let Ok(mut s) = sc.lock() {
            s.shutdown();
        }
    }
    // 3. Drop the request queues so each worker thread's recv loop sees EOF
    //    and exits (dropping its own sidecar clone, which reaps the child).
    w.senders.clear();
    // 4. Join: bounded, because the children were killed in step 1.
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    handles.append(&mut w.handles);
    for h in handles {
        let _ = h.join();
    }
}

impl VmHost for Vm {
    fn note_box_dirty(&mut self, addr: usize) {
        self.heap.note_box_dirty(addr);
    }

    fn require_module(&mut self, path: &str) -> Value {
        self.require_module(path)
    }

    fn reload_module(&mut self, path: &str) -> bool {
        self.reload_module(path)
    }

    fn promote_generation(&mut self) {
        // Called after each HTTP request: promote whatever the handler kept
        // (globals, channels, closures) and reclaim the request's garbage.
        self.promote_and_reclaim(None);
    }

    fn take_uncaught_exception(&mut self) -> Option<Value> {
        self.uncaught_exception.take()
    }

    fn throw_exception(&mut self, exc: Value) {
        match self.throw_value(exc) {
            ThrowResult::Jump(p) => self.native_throw_jump = Some(p),
            ThrowResult::EndDispatch | ThrowResult::Abort => {}
        }
    }

    fn this_value(&self) -> Value {
        self.native_this.clone().unwrap_or(Value::undefined())
    }

    fn resolve_promise(&mut self, promise: &Value, value: Value) {
        if let Some(p) = promise.as_promise() {
            self.resolve_promise(p, value);
        }
    }

    fn reject_promise(&mut self, promise: &Value, value: Value) {
        if let Some(p) = promise.as_promise() {
            self.reject_promise(p, value);
        }
    }

    fn then(&mut self, promise: &Value, callback: Value, on_rejected: Option<Value>) -> Value {
        self.then(promise, callback, on_rejected)
    }

    fn schedule_timer(&mut self, callback: Value, ms: f64, period: Option<f64>) -> u64 {
        self.schedule_timer(callback, ms, period)
    }

    fn clear_timer(&mut self, id: u64) {
        self.clear_timer(id)
    }

    fn queue_microtask(&mut self, callback: Value) {
        // An already-fulfilled promise: `then` on it enqueues the callback
        // as the next microtask (Node's microtask ordering).
        let p = self.new_promise();
        VmHost::resolve_promise(self, &p, Value::undefined());
        VmHost::then(self, &p, callback, None);
    }

    fn python_call(&mut self, src: &str, func: &str, args: &[Value], base: usize, cap: usize) -> Value {
        self.vm_python_call(src, func, args, base, cap)
    }

    fn spawn_fn(&mut self, f: &Value, args: &[Value]) -> Value {
        self.vm_spawn_fn(f, args)
    }

    fn finalize_python_embed(&mut self) -> Value {
        let mut m = HashMap::new();
        if !crate::python_embed::embed_enabled() {
            let why = if crate::python_embed::is_finalized() {
                "embed mode is not active (the interpreter was finalized)".to_string()
            } else {
                "embed mode is not active".to_string()
            };
            m.insert("finalized".to_string(), Value::bool(false));
            m.insert("error".to_string(), Value::string(why));
            m.insert("liveBackends".to_string(), Value::number(0.0));
            return Value::object(m);
        }
        // Release this VM's backends (module refs released, worker threads
        // joined) so the finalize guard can pass; other VMs' backends still
        // block finalization and surface in `error` / `liveBackends`.
        self.shutdown_python_workers();
        let result = crate::python_embed::finalize_interpreter();
        let (finalized, error) = match result {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e),
        };
        m.insert("finalized".to_string(), Value::bool(finalized));
        m.insert("error".to_string(), Value::string(error));
        m.insert(
            "liveBackends".to_string(),
            Value::number(crate::python_embed::live_backends() as f64),
        );
        Value::object(m)
    }

    fn drive_pending(&mut self) {
        self.drive_pending_inner();
    }

    fn pump_async(&mut self) {
        self.pump_async_once();
    }

    fn new_promise(&mut self) -> Value {
        // Stamp the owner: a settlement from another thread routes back to
        // this VM's event loop instead of being enqueued here.
        Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Pending,
            continuations: Vec::new(),
            owner: Some(self.wake_tx.clone()),
        })))
    }

    fn wake_handle(&self) -> Option<Arc<WakeHandle>> {
        Some(self.wake_tx.clone())
    }

    fn park_cross_waiter(&mut self, p: &Value) {
        self.cross_waiters.push(p.clone());
    }

    fn note_gc_dirty(&mut self, d: RcDirtyRef) {
        self.note_rc_dirty(d);
    }

    fn call_value(&mut self, callee: &Value, args: &[Value]) -> Value {
        self.call_value_with_this(callee, None, args)
    }

    fn call_value_with_this(
        &mut self,
        callee: &Value,
        this_arg: Option<Value>,
        args: &[Value],
    ) -> Value {
        // Host-initiated calls (HTTP request handlers, timers, tests,
        // Function.prototype.call/apply) may run outside `run()`'s guard —
        // allocate into the VM's heap regardless so the per-unit escape
        // analysis can reclaim the caller's garbage.
        let heap_ptr: *mut ArenaHeap = &mut self.heap;
        let _g = HeapGuard::set(heap_ptr);
        let argc = args.len();
        let this_slot = if let Some(t) = this_arg {
            self.push(t);
            Some(self.stack.len())
        } else {
            None
        };
        for a in args {
            self.push(a.clone());
        }
        // The receiver sits directly below the args (CallMethod layout).
        let this_slot = this_slot.map(|_| self.stack.len() - argc - 1);
        // Same stack-space guard as dispatch_call: the operand stack is a
        // fixed array, so a frame landing near the top fails gracefully.
        let base_slot = self.stack.len() - argc;
        if base_slot + FRAME_BUDGET > STACK_SIZE {
            for _ in 0..argc {
                self.pop();
            }
            return Value::undefined();
        }
        if let Some(f) = callee.as_function() {
            let cells_len = self.cells_stack.len();
            self.cells_stack.push(f.cells.clone());
            let arg_values = if f.uses_args != 0 {
                Some((0..argc).map(|i| self.stack.at(base_slot + i).clone()).collect())
            } else {
                None
            };
            self.call_stack.push(CallFrame {
                return_addr: self.bytecode.len(),
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                // Host-initiated calls (Promise.then, timers, ...) always
                // consume the result.
                keep_result: true,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor: false,
            });
            self.mark_param_kinds(base_slot);
            if f.program != self.program_id {
                self.load_program(f.program);
            }
            self.dispatch(f.ptr)
        } else if let Some(f) = callee.as_native() {
            // Snapshot the receiver before the pops; a native's `this` comes
            // from `this_value`, and the outer receiver must be restored
            // afterwards so re-entrant calls still see their own.
            let native_this_val = this_slot.map(|ts| self.stack.at(ts).clone());
            let mut args: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
            args.reverse();
            if this_slot.is_some() {
                self.pop();
            }
            let saved_this = self.native_this.take();
            self.native_this = native_this_val;
            let result = f(&args, self);
            self.native_this = saved_this;
            // A native threw: swallow the (meaningless) result; the exception
            // state (handler unwind or uncaught flag) is what the caller
            // observes afterwards.
            if self.native_throw_jump.take().is_some() || self.uncaught_exception.is_some() {
                Value::undefined()
            } else {
                result
            }
        } else if let Some(fn_ptr) = callee.as_number() {
            let cells_len = self.cells_stack.len();
            self.call_stack.push(CallFrame {
                return_addr: self.bytecode.len(),
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values: None,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                keep_result: true,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor: false,
            });
            self.dispatch(fn_ptr as usize)
        } else {
            Value::undefined()
        }
    }
}

/// `Array.prototype` method reads for `arr.name` — each method is a native
/// that captures the array `Value` (a bit copy; arrays are arena boxes) and
/// mutates through the array's `RefCell`. JS semantics verified against Node:
/// `push` returns the new length, `unshift` returns the new length, `shift`/`pop`
/// return the removed element (undefined on empty), `slice`/`concat` return new
/// arrays (int elements stay packed), `indexOf`/`includes` use strict
/// equality (includes finds NaN, indexOf never does), `map`/`forEach` invoke
/// their callback with `(element, index, array)` and return a new array /
/// undefined.
/// JS `String.prototype.trim` whitespace: the WhiteSpace + LineTerminator set
/// (`\u0009-\u000D`, `\u0020`, `\u00A0`, `\u1680`, `\u2000-\u200A`,
/// `\u2028`, `\u2029`, `\u202F`, `\u205F`, `\u3000`, `\uFEFF`). Rust's
/// `is_whitespace` covers every entry except `\uFEFF` (the BOM), which JS
/// trims — so strip it explicitly.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// Stable bottom-up merge sort (V8's sort is stable; ES2019 requires it).
/// `cmp` must return `Less`/`Equal`/`Greater`; equal elements keep their
/// input order.
fn stable_merge_sort<T: Clone>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> std::cmp::Ordering) {
    let n = v.len();
    if n <= 1 {
        return;
    }
    let mut aux: Vec<T> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        let mut i = 0;
        while i < n {
            let lo = i;
            let mid = (i + width).min(n);
            let hi = (i + 2 * width).min(n);
            aux.clear();
            let (mut a, mut b) = (lo, mid);
            while a < mid && b < hi {
                if cmp(&v[a], &v[b]) != std::cmp::Ordering::Greater {
                    aux.push(v[a].clone());
                    a += 1;
                } else {
                    aux.push(v[b].clone());
                    b += 1;
                }
            }
            while a < mid {
                aux.push(v[a].clone());
                a += 1;
            }
            while b < hi {
                aux.push(v[b].clone());
                b += 1;
            }
            for (k, x) in aux.iter().enumerate() {
                v[lo + k] = x.clone();
            }
            i = hi;
        }
        width *= 2;
    }
}

/// First index of `needle` in `hay` at or after `from` (char positions, like
/// JS indexOf operates on code units — for the ASCII corpus both agree). An
/// empty needle matches at `min(from, len)`. `None` when absent.
fn char_index_of(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    let n = hay.len();
    let m = needle.len();
    if m == 0 {
        return Some(from.min(n));
    }
    if from > n || m > n - from {
        return None;
    }
    let mut i = from;
    while i <= n - m {
        if hay[i..i + m] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Last index of `needle` in `hay` at or before `start` (char positions). An
/// empty needle matches at `min(start, len)`. `None` when absent.
fn char_last_index_of(hay: &[char], needle: &[char], start: usize) -> Option<usize> {
    let n = hay.len();
    let m = needle.len();
    if m == 0 {
        return Some(start.min(n));
    }
    if m > n {
        return None;
    }
    let mut i = start.min(n - m);
    loop {
        if hay[i..i + m] == *needle {
            return Some(i);
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

/// Expand JS `$` patterns in a replacement string: `$$` → `$`, `$&` → the
/// matched text, `$`` → text before the match, `$'` → text after the match.
/// `$n` digits are kept literally (no capture groups without regex).
fn expand_replacement(
    repl: &str,
    matched: &str,
    before: &str,
    after: &str,
    caps: &[Option<(usize, usize)>],
    hay: &[char],
) -> String {
    let mut out = String::with_capacity(repl.len() + matched.len());
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let peeked = chars.peek().copied();
            match peeked {
                Some('$') => {
                    out.push('$');
                    chars.next();
                }
                Some('&') => {
                    out.push_str(matched);
                    chars.next();
                }
                Some('`') => {
                    out.push_str(before);
                    chars.next();
                }
                Some('\'') => {
                    out.push_str(after);
                    chars.next();
                }
                Some(d) if d.is_ascii_digit() => {
                    // `$1`..`$99` capture references — the capture text, or
                    // "" when the group didn't participate or is out of
                    // range. A two-digit `$nn` is used only when `nn` names a
                    // real capture (ES spec); otherwise the single digit.
                    let n_groups = (caps.len().saturating_sub(1)) / 2;
                    let mut num = (d as u8 - b'0') as usize;
                    let mut consumed = 1;
                    let nxt = chars.peek().copied();
                    if let Some(d2) = nxt {
                        if d2.is_ascii_digit() {
                            let two = num * 10 + (d2 as u8 - b'0') as usize;
                            if two <= n_groups {
                                num = two;
                                consumed = 2;
                            }
                        }
                    }
                    let text = if num <= n_groups {
                        match caps.get(num).and_then(|c| *c) {
                            Some((x, y)) => hay.get(x..y).map(|c| c.iter().collect::<String>()),
                            None => None,
                        }
                    } else {
                        None
                    };
                    match text {
                        Some(t) => out.push_str(&t),
                        None => {
                            out.push('$');
                            out.push(d);
                        }
                    }
                    for _ in 0..consumed {
                        chars.next();
                    }
                }
                _ => out.push('$'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Regular expressions — exec/test/match/replace/search/split runtime
// ---------------------------------------------------------------------------

/// Convert a UTF-16 code-unit offset (V8's `lastIndex` unit) back to a char
/// index. Surrogate pairs count double, so a `lastIndex` set mid-pair lands
/// on the pair's start char.
fn utf16_to_char(chars: &[char], u16pos: usize) -> usize {
    let mut acc = 0usize;
    for (i, c) in chars.iter().enumerate() {
        if acc >= u16pos {
            return i;
        }
        acc += c.len_utf16();
    }
    chars.len()
}

/// One exec/test attempt: run the regex against `arg` (coerced via ToString)
/// at `lastIndex` (only honored for /g /y, like Node), advancing `lastIndex`
/// on success (empty matches advance one code unit so /g terminates) and
/// resetting it to 0 when a /g search fails.
struct RegexExecOutcome {
    matched: Option<regex::Match>,
    hay: Vec<char>,
    hay_text: String,
}

fn regex_exec_core(st: &Arc<Mutex<RegexState>>, arg: &Value) -> RegexExecOutcome {
    let mut g = st.lock().unwrap_or_else(|g| g.into_inner());
    let hay_text = to_string_js(arg);
    let hay: Vec<char> = hay_text.chars().collect();
    let flags = g.compiled.flags;
    let use_last = flags.global || flags.sticky;
    let start_char = if use_last {
        utf16_to_char(&hay, g.last_index)
    } else {
        0
    };
    let matched = regex::search(&g.compiled, &hay, start_char);
    match &matched {
        None => {
            if flags.global {
                g.last_index = 0;
            }
        }
        Some(m) => {
            if use_last {
                g.last_index = if m.end == m.start {
                    regex::char_pos_to_utf16(&hay, m.start) + 1
                } else {
                    regex::char_pos_to_utf16(&hay, m.end)
                };
            }
        }
    }
    RegexExecOutcome {
        matched,
        hay,
        hay_text,
    }
}

/// Build the exec result: an object with `0` = full match, `1..G` = capture
/// texts (undefined when a group didn't participate), plus `length`,
/// `index` (UTF-16), and `input`. Object-shaped rather than array-shaped
/// (the engine has no getter-backed array subtypes) — element reads, length,
/// index and input all work; spread/`Array.isArray` do not (documented).
fn regex_exec_value(st: &Arc<Mutex<RegexState>>, arg: &Value) -> Value {
    let out = regex_exec_core(st, arg);
    let Some(m) = out.matched else {
        return Value::null();
    };
    let hay = &out.hay;
    let n_groups = (m.caps.len().saturating_sub(1)) / 2;
    let mut entries: Vec<(String, Value)> = Vec::with_capacity(n_groups + 4);
    let full: String = hay[m.start..m.end].iter().collect();
    entries.push(("0".to_string(), Value::string(full)));
    for g in 1..=n_groups {
        let v = match m.caps.get(g).and_then(|c| *c) {
            Some((x, y)) => Value::string(hay[x..y].iter().collect()),
            None => Value::undefined(),
        };
        entries.push((g.to_string(), v));
    }
    entries.push(("length".to_string(), Value::int(n_groups as i64 + 1)));
    entries.push((
        "index".to_string(),
        Value::int(regex::char_pos_to_utf16(hay, m.start) as i64),
    ));
    entries.push(("input".to_string(), Value::string(out.hay_text)));
    Value::object_ordered(entries)
}

/// The regex value's own property surface: read-only flags, `lastIndex`
/// (mutable), and the exec/test/toString methods.
fn regex_prop(obj: &Value, name: &str) -> Value {
    let st = obj.as_regex().expect("regex_prop called on a regex").clone();
    let static_val = {
        let g = st.lock().unwrap_or_else(|g| g.into_inner());
        match name {
            "source" => Some(Value::string(g.compiled.pattern_source())),
            "flags" => Some(Value::string(g.compiled.flags.source())),
            "global" => Some(Value::bool(g.compiled.flags.global)),
            "ignoreCase" => Some(Value::bool(g.compiled.flags.ignore_case)),
            "multiline" => Some(Value::bool(g.compiled.flags.multiline)),
            "dotAll" => Some(Value::bool(g.compiled.flags.dot_all)),
            "sticky" => Some(Value::bool(g.compiled.flags.sticky)),
            "unicode" => Some(Value::bool(g.compiled.flags.unicode)),
            "lastIndex" => Some(Value::int(g.last_index as i64)),
            _ => None,
        }
    };
    if let Some(v) = static_val {
        return v;
    }
    match name {
        "exec" => Value::native(Arc::new(move |args, _vm| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            regex_exec_value(&st, &arg)
        })),
        "test" => Value::native(Arc::new(move |args, _vm| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(regex_exec_core(&st, &arg).matched.is_some())
        })),
        "toString" => Value::native(Arc::new(move |_args, _vm| {
            let g = st.lock().unwrap_or_else(|g| g.into_inner());
            Value::string(g.compiled.to_source_string())
        })),
        _ => Value::undefined(),
    }
}

// ---------------------------------------------------------------------------
// Number.prototype formatting — V8-exact toFixed / toPrecision / toString(radix)
// ---------------------------------------------------------------------------
// Rust's {:.*} rounds half-to-even on the decimal digits of the binary value;
// JS rounds the EXACT binary value half-away-from-zero (so (1.005).toFixed(2)
// is "1.00" and (2.5).toFixed(0) is "3"). toString(radix != 10) is the
// shortest-round-trip digit string (V8 prints "7b.74bc6a7ef9dc" for
// 123.456.toString(16), not the full 13-digit expansion). Everything below
// works on the exact dyadic value via a tiny arbitrary-precision integer.

type Big = Vec<u64>; // little-endian limbs

fn big_from_u64(v: u64) -> Big {
    vec![v]
}

fn big_trim(b: &mut Big) {
    while b.len() > 1 && *b.last().unwrap() == 0 {
        b.pop();
    }
}

fn big_is_zero(b: &Big) -> bool {
    b.iter().all(|&l| l == 0)
}

fn big_shl(b: &mut Big, bits: u64) {
    if bits == 0 {
        return;
    }
    let words = (bits / 64) as usize;
    let rem = (bits % 64) as u32;
    if words > 0 {
        let mut nb = vec![0u64; b.len() + words];
        nb[words..].copy_from_slice(b);
        *b = nb;
    }
    if rem > 0 {
        b.push(0);
        for i in (1..b.len()).rev() {
            b[i] = (b[i] << rem) | (b[i - 1] >> (64 - rem));
        }
        b[0] <<= rem;
    }
    big_trim(b);
}

fn big_shr(b: &mut Big, bits: u64) {
    if bits == 0 {
        return;
    }
    let words = (bits / 64) as usize;
    let rem = (bits % 64) as u32;
    if words >= b.len() {
        *b = vec![0];
        return;
    }
    if words > 0 {
        b.drain(..words);
    }
    if rem > 0 {
        for i in 0..b.len() - 1 {
            b[i] = (b[i] >> rem) | (b[i + 1] << (64 - rem));
        }
        *b.last_mut().unwrap() >>= rem;
    }
    big_trim(b);
}

fn big_mul_small(b: &mut Big, m: u64) {
    let mut carry = 0u64;
    for l in b.iter_mut() {
        let cur = (*l as u128) * (m as u128) + (carry as u128);
        *l = cur as u64;
        carry = (cur >> 64) as u64;
    }
    if carry > 0 {
        b.push(carry);
    }
}

fn big_add(a: &mut Big, b: &Big) {
    let mut carry = 0u64;
    let n = a.len().max(b.len());
    a.resize(n, 0);
    for i in 0..n {
        let av = a[i];
        let bv = if i < b.len() { b[i] } else { 0 };
        let (s1, c1) = av.overflowing_add(bv);
        let (s2, c2) = s1.overflowing_add(carry);
        a[i] = s2;
        carry = (c1 as u64) + (c2 as u64);
    }
    if carry > 0 {
        a.push(carry);
    }
    big_trim(a);
}

fn big_div_small_rem(b: &mut Big, d: u64) -> u64 {
    let mut rem = 0u64;
    for l in b.iter_mut().rev() {
        let cur = ((rem as u128) << 64) | (*l as u128);
        *l = (cur / d as u128) as u64;
        rem = (cur % d as u128) as u64;
    }
    big_trim(b);
    rem
}

/// floor(b / 2^k), valid when the result fits a u64 (callers ensure b < 2^k * 36).
fn big_shr_small(b: &Big, k: u64) -> u64 {
    let words = (k / 64) as usize;
    let rem = (k % 64) as u32;
    if words >= b.len() {
        return 0;
    }
    let mut v = b[words] >> rem;
    if rem > 0 && words + 1 < b.len() {
        v |= b[words + 1] << (64 - rem);
    }
    v
}

/// b &= (2^k - 1)
fn big_mask_low(b: &mut Big, k: u64) {
    if k == 0 {
        *b = vec![0];
        return;
    }
    let words = (k / 64) as usize;
    let rem = (k % 64) as u32;
    if words >= b.len() {
        return; // b < 2^(64*words) <= 2^k — nothing to mask
    }
    b.truncate(words + 1);
    if rem > 0 {
        b[words] &= (1u64 << rem) - 1;
    } else {
        b.truncate(words);
    }
    big_trim(b);
}

fn big_to_radix(b: &Big, radix: u32) -> String {
    if big_is_zero(b) {
        return "0".to_string();
    }
    let dig = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = b.clone();
    let mut rev = Vec::new();
    while !big_is_zero(&n) {
        let r = big_div_small_rem(&mut n, radix as u64);
        rev.push(dig[r as usize] as char);
    }
    rev.iter().rev().collect()
}

fn big_to_decimal(b: &Big) -> String {
    big_to_radix(b, 10)
}

/// (mantissa, unbiased exponent) such that x = mant * 2^(exp - 52), mant < 2^53.
fn decompose(x: f64) -> (u64, i64) {
    let bits = x.to_bits();
    let mut exp = ((bits >> 52) & 0x7ff) as i64;
    let mut mant = bits & ((1u64 << 52) - 1);
    if exp == 0 {
        exp = 1; // subnormal: no implicit leading 1
    } else {
        mant |= 1u64 << 52;
    }
    (mant, exp - 1023)
}

/// V8 `Double::NextDouble()`: the next representable f64 above `f`
/// (below for negatives), +Infinity for +Infinity.
fn next_double(f: f64) -> f64 {
    let bits = f.to_bits();
    if bits == 0x7ff0_0000_0000_0000 {
        return f; // +Infinity
    }
    let neg = bits >> 63 == 1;
    if neg && bits & 0x000f_ffff_ffff_ffff == 0 {
        return 0.0; // -0.0
    }
    f64::from_bits(if neg { bits - 1 } else { bits + 1 })
}

/// V8 `Double::Exponent()`: biased exponent field minus 1075, -1074 for
/// subnormals. Note the masking must happen BEFORE the shift — `(bits &
/// mask) >> 52`, not `bits & mask >> 52` (Rust precedence would shift first).
fn double_exponent(f: f64) -> i32 {
    let bits = f.to_bits();
    if bits & 0x7ff0_0000_0000_0000 == 0 {
        return -1074;
    }
    let biased = ((bits & 0x7ff0_0000_0000_0000) >> 52) as i32;
    biased - 1075
}

/// V8-exact `Number.prototype.toString(radix)` for radix != 10.
/// Faithful port of V8's `DoubleToRadixCString` (src/numbers/conversions.cc):
/// the fractional digits are computed with f64 arithmetic driven by `delta`
/// (half the distance to the next double), with round-to-even termination and
/// a back-tracing carry; the integer part pads zeros only when the value is
/// >= 2^53, then extracts digits by repeated `%`/`/`. All f64 operations must
/// stay in f64 — this is not shortest-round-trip, and it intentionally
/// reproduces V8's IEEE-arithmetic digit counts (e.g. 0.1.toString(16) emits
/// 14 digits, 1.5.toString(3) ends in "12" after round-to-even).
/// Verified against Node on 1518 (value, radix) pairs.
fn js_to_string_radix(x: f64, radix: u32) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let radix_f = radix as f64;

    let neg = x < 0.0;
    let v = x.abs();
    let mut integer = v.floor();
    let mut fraction = v - integer;

    // delta = 0.5 * (NextDouble(v) - v), clamped to the minimum denormal delta.
    let mut delta = 0.5 * (next_double(v) - v);
    delta = delta.max(next_double(0.0));

    // Fractional digits (only when the fraction is representable).
    let mut frac: Vec<u8> = Vec::new();
    if fraction >= delta {
        loop {
            fraction *= radix_f;
            delta *= radix_f;
            let digit = fraction as usize;
            frac.push(chars[digit]);
            fraction -= digit as f64;
            // Round to even.
            if (fraction > 0.5 || (fraction == 0.5 && (digit & 1) == 1))
                && fraction + delta > 1.0
            {
                // Back-trace already-written digits in case of carry-over.
                while let Some(&c) = frac.last() {
                    let d = if c > b'9' { c - b'a' + 10 } else { c - b'0' };
                    if d + 1 < radix as u8 {
                        let pos = frac.len() - 1;
                        frac[pos] = chars[(d + 1) as usize];
                        frac.truncate(pos + 1); // digits after the bump are dropped
                        break;
                    }
                    frac.pop(); // digit rolls to 0, carry continues
                }
                if frac.is_empty() {
                    integer += 1.0; // carried all the way to the integer part
                }
                break;
            }
            if fraction < delta {
                break;
            }
            if frac.len() > 4096 {
                break; // safety guard; never reached for finite f64
            }
        }
    }

    // Integer digits. The while loop only fires for values >= 2^53 (V8's
    // Exponent() > 0), padding the top digits with zeros; then extract the
    // remaining digits least-significant-first.
    let mut int_digits: Vec<u8> = Vec::new();
    while double_exponent(integer / radix_f) > 0 {
        integer /= radix_f;
        int_digits.push(b'0');
    }
    loop {
        let remainder = integer % radix_f;
        int_digits.push(chars[remainder as usize]);
        integer = (integer - remainder) / radix_f;
        if integer <= 0.0 {
            break;
        }
    }
    int_digits.reverse(); // emitted least-significant-first

    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.extend(int_digits.iter().map(|&c| c as char));
    if !frac.is_empty() {
        s.push('.');
        s.extend(frac.iter().map(|&c| c as char));
    }
    s
}

/// V8-exact `Number.prototype.toFixed(f)`: rounds the EXACT binary value
/// half-away-from-zero (1.005 -> "1.00", 2.5 -> "3").
fn js_to_fixed(x: f64, f: i64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x.abs() >= 1e21 {
        return js_number_to_string(x);
    }
    if x == 0.0 {
        let mut s = "0".to_string();
        if f > 0 {
            s.push('.');
            for _ in 0..f {
                s.push('0');
            }
        }
        return s;
    }
    let neg = x < 0.0;
    let a = x.abs();
    let (mant, exp) = decompose(a);
    // n = round_half_away(a * 10^f) = round_half_away(mant * 5^f * 2^(f + exp - 52))
    let mut num = big_from_u64(mant);
    for _ in 0..f {
        big_mul_small(&mut num, 5);
    }
    let k = 52 - f - exp;
    if k <= 0 {
        big_shl(&mut num, (-k) as u64);
    } else {
        let mut half = big_from_u64(1);
        big_shl(&mut half, (k - 1) as u64);
        big_add(&mut num, &half);
        big_shr(&mut num, k as u64);
    }
    let digits = big_to_decimal(&num);
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let fl = f as usize;
    if digits.len() <= fl {
        out.push('0');
        if fl > 0 {
            out.push('.');
            for _ in 0..(fl - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        let split = digits.len() - fl;
        out.push_str(&digits[..split]);
        if fl > 0 {
            out.push('.');
            out.push_str(&digits[split..]);
        }
    }
    out
}

/// V8-exact `Number.prototype.toPrecision(p)`.
fn js_to_precision(x: f64, p: i64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x == 0.0 {
        if p <= 1 {
            return "0".to_string();
        }
        let mut s = "0.".to_string();
        for _ in 1..p {
            s.push('0');
        }
        return s;
    }
    let neg = x < 0.0;
    let a = x.abs();
    let (mant, exp) = decompose(a);
    let shift = exp - 52;
    let mut int_big = big_from_u64(0);
    let mut frac_mant = 0u64;
    let mut k = 0u64;
    if shift >= 0 {
        int_big = big_from_u64(mant);
        big_shl(&mut int_big, shift as u64);
    } else {
        k = (-shift) as u64;
        if k >= 64 {
            frac_mant = mant;
        } else {
            int_big = big_from_u64(mant >> k);
            frac_mant = mant & ((1u64 << k as u32) - 1);
        }
    }
    let int_digits = big_to_decimal(&int_big);
    let int_digits: Vec<u8> = int_digits.bytes().map(|b| b - b'0').collect();
    // exact decimal expansion of the fraction part (terminates: dyadic)
    let mut num_frac = big_from_u64(frac_mant);
    let mut frac: Vec<u8> = Vec::new();
    let mut guard = 0;
    while guard < 2000 {
        if big_is_zero(&num_frac) {
            break;
        }
        big_mul_small(&mut num_frac, 10);
        let d = big_shr_small(&num_frac, k) as u8;
        big_mask_low(&mut num_frac, k);
        frac.push(d);
        guard += 1;
    }
    let mut sig: Vec<u8>; // significant digits (no leading zeros)
    let n: i64;
    if int_digits != [0] {
        sig = int_digits;
        n = sig.len() as i64;
        // append fraction digits (leading zeros included) up to p+1 significant
        let need = (p + 1) as usize;
        sig.extend(frac.iter().take(need.saturating_sub(sig.len())));
    } else {
        let first = frac.iter().position(|&d| d != 0);
        match first {
            Some(idx) => {
                sig = frac[idx..].to_vec();
                n = -(idx as i64);
            }
            None => {
                // all zeros — unreachable (x != 0), but keep it safe
                sig = vec![0];
                n = 0;
            }
        }
        // pad if the expansion terminated before p+1 significant digits
        while (sig.len() as i64) < p + 1 {
            sig.push(0);
        }
    }
    // Round `sig` to p digits, half away from zero (exact digits).
    let mut q: Vec<u8>;
    if sig.len() as i64 <= p {
        q = sig;
        while (q.len() as i64) < p {
            q.push(0);
        }
    } else {
        let keep = &sig[..p as usize];
        let next = sig[p as usize];
        let mut kd = keep.to_vec();
        if next >= 5 {
            let mut i = kd.len();
            while i > 0 {
                i -= 1;
                if kd[i] < 9 {
                    kd[i] += 1;
                    break;
                }
                kd[i] = 0;
            }
            if kd.iter().all(|&d| d == 0) {
                kd.insert(0, 1); // carry past the front: 999... -> 1000...
            }
        }
        q = kd;
    }
    let qd = q.len() as i64;
    let n2 = qd + (n - p);
    // significant digits for display (strip trailing zeros, pad to exactly p)
    let mut s = q.clone();
    while s.len() > 1 && *s.last().unwrap() == 0 {
        s.pop();
    }
    while (s.len() as i64) < p {
        s.push(0);
    }
    let dstr: String = s.iter().map(|&d| (b'0' + d) as char).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    // Exponential boundaries per spec (21.1.3.3): e >= p or e < -6, where
    // e = n2 - 1 is the decimal exponent. So p < n2 (e >= p) or n2 <= -6
    // (e < -6). Note n2 <= -6, not < -6: 1e-7 (n2 = -6) is exponential while
    // 1e-6 (n2 = -5) is fixed. (toFixed's 1e21 rule does NOT apply here —
    // (1e25).toPrecision(30) is fixed "10000000000000000905969664.0000".)
    if p < n2 || n2 <= -6 {
        // exponential: d.ddd e±X
        out.push(dstr.as_bytes()[0] as char);
        if p > 1 {
            out.push('.');
            out.push_str(&dstr[1..]);
        }
        let e = n2 - 1;
        out.push('e');
        if e < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        out.push_str(&e.abs().to_string());
    } else if n2 <= 0 {
        out.push_str("0.");
        for _ in 0..(-n2) {
            out.push('0');
        }
        out.push_str(&dstr);
    } else if (n2 as usize) >= dstr.len() {
        out.push_str(&dstr);
        for _ in 0..(n2 - dstr.len() as i64) {
            out.push('0');
        }
    } else {
        out.push_str(&dstr[..n2 as usize]);
        out.push('.');
        out.push_str(&dstr[n2 as usize..]);
    }
    out
}

fn to_integer_or_infinity(v: &Value) -> f64 {
    let n = v.to_number();
    if n.is_nan() {
        0.0
    } else {
        n.trunc()
    }
}

fn number_prop(obj: &Value, name: &str) -> Value {
    let n = match obj.as_number() {
        Some(n) => n,
        None => obj.as_int().map(|i| i as f64).unwrap_or(f64::NAN),
    };
    match name {
        "toString" => Value::native(Arc::new(move |args, vm| {
            let radix = match args.first() {
                Some(v) if v.is_undefined() => 10.0,
                Some(v) => to_integer_or_infinity(v),
                None => 10.0,
            };
            if radix < 2.0 || radix > 36.0 {
                vm.throw_exception(Value::string(
                    "RangeError: toString() radix argument must be between 2 and 36".to_string(),
                ));
                return Value::undefined();
            }
            let r = radix as u32;
            if r == 10 {
                Value::string(js_number_to_string(n))
            } else {
                Value::string(js_to_string_radix(n, r))
            }
        })),
        "toFixed" => Value::native(Arc::new(move |args, vm| {
            let f = match args.first() {
                Some(v) if v.is_undefined() => 0.0,
                Some(v) => to_integer_or_infinity(v),
                None => 0.0,
            };
            if f < 0.0 || f > 100.0 {
                vm.throw_exception(Value::string(
                    "RangeError: toFixed() digits argument must be between 0 and 100".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(js_to_fixed(n, f as i64))
        })),
        "toPrecision" => Value::native(Arc::new(move |args, vm| {
            let p = match args.first() {
                Some(v) if v.is_undefined() => {
                    return Value::string(js_number_to_string(n));
                }
                Some(v) => to_integer_or_infinity(v),
                None => {
                    return Value::string(js_number_to_string(n));
                }
            };
            if p < 1.0 || p > 100.0 {
                vm.throw_exception(Value::string(
                    "RangeError: toPrecision() argument must be between 1 and 100".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(js_to_precision(n, p as i64))
        })),
        _ => Value::undefined(),
    }
}

/// `Map`/`Set` instance reads (`m.get`, `s.add`, `m.size`, …) — each method
/// is a native that captures the specific container `Value` and works its
/// SameValueZero entry table through the box's `RefCell`, mirroring the
/// `array_prop` pattern (natives receive no `this`). Unknown names return
/// None so the caller falls through to the normal shape/proto read.
/// Value display for "is not iterable" errors. V8 formats the *source text*
/// of the iterable (`for (x of o)` → "o is not iterable"), which a compiled
/// VM cannot reproduce — this is the stable value-based form, matching V8
/// exactly for primitives and the empty-object `{}` case.
fn iterable_display(v: &Value) -> String {
    if v.is_undefined() {
        return "undefined".to_string();
    }
    if v.is_null() {
        return "null".to_string();
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if v.is_number() || v.is_int() {
        return format!("{}", v);
    }
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if v.is_object() {
        let empty = match v.as_object() {
            Some(od) => od.borrow().shape.len() == 0,
            None => true,
        };
        return if empty {
            "{}".to_string()
        } else {
            "{...}".to_string()
        };
    }
    format!("{}", v)
}

/// Own enumerable properties of `src` in insertion order, for object spread
/// `{...src}`: plain objects walk the shape (tombstones skipped, cells
/// unwrapped); arrays yield their indices `0..len`; strings yield their
/// character indices (all match JS). Everything else — numbers, booleans,
/// functions, null, undefined — has no own enumerable props, so `None` makes
/// the spread a no-op. Map/Set return `None` too: in JS they have no own
/// enumerable props.
fn object_spread_pairs(src: &Value) -> Option<Vec<(String, Value)>> {
    if let Some(od) = src.as_object() {
        if od.borrow().container != 0 {
            return None;
        }
        let od = od.borrow();
        let mut out = Vec::with_capacity(od.shape.len());
        for (k, off) in od.shape.keys_by_offset() {
            let off = off as usize;
            if !od.deleted[off] {
                out.push((k.clone(), unwrap_cell(od.values[off].clone())));
            }
        }
        return Some(out);
    }
    if let Some(arr) = src.as_array() {
        let arr = arr.borrow();
        let mut out = Vec::with_capacity(arr.len());
        for i in 0..arr.len() {
            out.push((i.to_string(), arr.get(i)));
        }
        return Some(out);
    }
    if let Some(s) = src.as_str() {
        let mut out = Vec::with_capacity(s.chars().count());
        for (i, ch) in s.chars().enumerate() {
            out.push((i.to_string(), Value::string(ch.to_string())));
        }
        return Some(out);
    }
    None
}

/// `key in obj` existence probe: `Some(true/false)` for object/function/
/// array targets (own + prototype chain), `None` for everything else
/// (primitives and Map/Set), which the caller reports as a TypeError.
fn in_operator_probe(obj: &Value, key: &str) -> Option<bool> {
    if let Some(od) = obj.as_object() {
        if od.borrow().container != 0 {
            return None;
        }
        let mut cur = obj.clone();
        for _ in 0..1024 {
            let Some(c) = cur.as_object() else { break };
            let (hit, next) = {
                let b = c.borrow();
                (b.shape.get(key).is_some_and(|off| !b.deleted[off as usize]), b.proto.clone())
            };
            if hit {
                return Some(true);
            }
            cur = next;
        }
        return Some(false);
    }
    if let Some(arr) = obj.as_array() {
        // Canonical array-index keys (`0 in a`), then the synthesized
        // prototype methods (`"map" in []`). Non-index keys on an array
        // resolve through the same synthesized surface.
        if let Ok(n) = key.parse::<usize>() {
            if n < arr.borrow().len() {
                return Some(true);
            }
        }
        // No Array.prototype object exists, so method names probe the
        // synthesized surface conservatively.
        return Some(!array_prop(obj, key).is_undefined());
    }
    if let Some(f) = obj.as_function() {
        return Some(
            f.props
                .borrow()
                .as_ref()
                .is_some_and(|p| p.borrow().contains_key(key)),
        );
    }
    None
}

/// A Map/Set *computed* property that cannot live on the prototype as a
/// shared native: `size` must read the instance's table (JS exposes it as a
/// getter, which this engine doesn't model), so it is synthesized per read.
/// Every other method lives once on Map.prototype / Set.prototype and reads
/// its instance from `this` (see [`container_method_native`]).
fn container_prop(obj: &Value, prop: &Value) -> Option<Value> {
    if prop.as_str() == Some("size") {
        // Only Map/Set have a `size`; Error (3) and Date (4) instances read
        // their props through the normal proto walk instead.
        let c = obj
            .as_object()
            .map(|o| o.borrow().container)
            .unwrap_or(0);
        if c == 1 || c == 2 {
            return Some(Value::int(container_len(obj)));
        }
    }
    None
}

/// One shared Map/Set method native, installed once on the prototype and
/// used by every instance: the receiver (`m`, `s`) is read from
/// `VmHost::this_value`, which `dispatch_call` stashes for the duration of
/// the native call. Extracting a method — `let g = m.get; g(k)` — gets
/// `this = undefined`, exactly like Node.
fn container_method_native(name: &str) -> Value {
    match name {
        "get" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            container_get(&m, &k)
        })),
        "set" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            let v = args.get(1).cloned().unwrap_or(Value::undefined());
            container_insert(&m, k, v, vm);
            m
        })),
        "add" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            // Set values are the keys themselves, so `values()`/`entries()`
            // (which read the stored value) yield the element.
            container_insert(&m, k.clone(), k, vm);
            m
        })),
        "has" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(container_contains(&m, &k))
        })),
        "delete" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(container_remove(&m, &k, vm))
        })),
        "clear" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_clear(&m, vm);
            Value::undefined()
        })),
        "keys" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_keys(&m)
        })),
        "values" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_values(&m)
        })),
        "entries" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_entries(&m)
        })),
        "forEach" => Value::native(Arc::new(|args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if !(cb.is_function() || cb.is_native()) {
                return Value::undefined();
            }
            let m = vm.this_value();
            // Live walk, not a snapshot: the order length is captured once,
            // but each step re-reads the table, so entries deleted mid-walk
            // are skipped (tombstoned) and entries added during the walk are
            // seen if they land before the walk finishes — Node's spec
            // latitude. A delete-triggered compaction shrinking the order
            // list just ends the walk early.
            let n = {
                let Some(od) = m.as_object() else {
                    return Value::undefined();
                };
                let od = od.borrow();
                od.entries.as_ref().map_or(0, |cd| cd.order_len())
            };
            let mut i = 0usize;
            while i < n {
                let (k, v) = {
                    let Some(od) = m.as_object() else {
                        break;
                    };
                    let od = od.borrow();
                    let Some(cd) = od.entries.as_ref() else {
                        break;
                    };
                    if i >= cd.order_len() {
                        break;
                    }
                    match cd.order_pair(i) {
                        Some(pair) => pair,
                        None => {
                            i += 1;
                            continue;
                        }
                    }
                };
                // Node calls `cb(value, key, map)` — for a Set both value and
                // key are the element, so the same shape serves both.
                vm.call_value(&cb, &[v.clone(), k.clone(), m.clone()]);
                i += 1;
            }
            Value::undefined()
        })),
        _ => unreachable!("unknown container method: {}", name),
    }
}

fn container_len(m: &Value) -> i64 {
    match m.as_object() {
        Some(od) => od.borrow().entries.as_ref().map_or(0, |e| e.len() as i64),
        None => 0,
    }
}

fn container_get(m: &Value, k: &Value) -> Value {
    let Some(od) = m.as_object() else {
        return Value::undefined();
    };
    let od = od.borrow();
    match od.entries.as_ref() {
        Some(e) => e.get(k).cloned().unwrap_or(Value::undefined()),
        None => Value::undefined(),
    }
}

fn container_contains(m: &Value, k: &Value) -> bool {
    let Some(od) = m.as_object() else {
        return false;
    };
    let od = od.borrow();
    od.entries.as_ref().is_some_and(|e| e.contains(k))
}

fn container_insert(m: &Value, k: Value, v: Value, vm: &mut dyn VmHost) {
    let Some(od) = m.as_object() else {
        return;
    };
    // The box may be in the old generation: a young value stored into it
    // must survive the next young collection, so flag it for the dirty scan.
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    od.entries.get_or_insert_with(Default::default).insert(k, v);
}

fn container_remove(m: &Value, k: &Value, vm: &mut dyn VmHost) -> bool {
    let Some(od) = m.as_object() else {
        return false;
    };
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    match od.entries.as_mut() {
        Some(e) => e.remove(k),
        None => false,
    }
}

fn container_clear(m: &Value, vm: &mut dyn VmHost) {
    let Some(od) = m.as_object() else {
        return;
    };
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    if let Some(e) = od.entries.as_mut() {
        e.clear();
    }
}

/// Live `(key, value)` pairs in insertion order (values are the elements
/// themselves for Sets).
fn container_pairs(m: &Value) -> Vec<(Value, Value)> {
    let Some(od) = m.as_object() else {
        return Vec::new();
    };
    let od = od.borrow();
    match od.entries.as_ref() {
        Some(cd) => cd.iter().collect(),
        None => Vec::new(),
    }
}

/// `m.keys()` — array snapshot of the keys in insertion order. (The engine
/// has no iterator protocol; the PRD target is the array shape.)
fn container_keys(m: &Value) -> Value {
    let mut out = Vec::new();
    for (k, _) in container_pairs(m) {
        out.push(k);
    }
    Value::array(out)
}

fn container_values(m: &Value) -> Value {
    let mut out = Vec::new();
    for (_, v) in container_pairs(m) {
        out.push(v);
    }
    Value::array(out)
}

fn container_entries(m: &Value) -> Value {
    let mut out = Vec::new();
    for (k, v) in container_pairs(m) {
        out.push(Value::array(vec![k, v]));
    }
    Value::array(out)
}

fn array_prop(obj: &Value, name: &str) -> Value {
    let arr = obj.clone();
    match name {
        "length" => Value::int(
            arr.as_array()
                .map(|ad| ad.borrow().len() as i64)
                .unwrap_or(0),
        ),
        "push" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut ad = ad.borrow_mut();
                for a in args {
                    ad.push(a.clone());
                }
                Value::int(ad.len() as i64)
            } else {
                Value::undefined()
            }
        })),
        "join" => Value::native(Arc::new(move |args, _vm| {
            let sep = match args.first() {
                Some(v) if v.is_undefined() || v.is_null() => ",".to_string(),
                Some(v) => to_string_js(v),
                None => ",".to_string(),
            };
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                // Packed-int fast path: format i64s directly, no Value boxing.
                match &*ad {
                    alloy_core::value::ArrayData::Ints(vs) => {
                        let mut out = String::with_capacity(vs.len() * 3);
                        for (i, n) in vs.iter().enumerate() {
                            if i > 0 { out.push_str(&sep); }
                            out.push_str(&n.to_string());
                        }
                        return Value::string(out);
                    }
                    alloy_core::value::ArrayData::Values(vs) => {
                        let mut parts = Vec::with_capacity(vs.len());
                        for e in vs.iter() {
                            if e.is_null() || e.is_undefined() {
                                parts.push(String::new());
                            } else {
                                parts.push(to_string_js(e));
                            }
                        }
                        return Value::string(parts.join(&sep));
                    }
                }
            } else {
                Value::undefined()
            }
        })),
        "pop" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                ad.borrow_mut().pop()
            } else {
                Value::undefined()
            }
        })),
        "shift" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                ad.borrow_mut().shift()
            } else {
                Value::undefined()
            }
        })),
        "unshift" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let items: Vec<Value> = args.to_vec();
                let len = ad.borrow_mut().unshift_front(&items);
                Value::int(len as i64)
            } else {
                Value::undefined()
            }
        })),
        "slice" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let n = ad.len() as i64;
                let to_i64 = |v: &Value| -> i64 {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                };
                let start = match args.first() {
                    Some(v) if v.is_undefined() => 0i64,
                    Some(v) => {
                        let mut s = to_i64(v);
                        if s < 0 {
                            s = (n + s).max(0);
                        }
                        s.min(n)
                    }
                    None => 0,
                };
                let end = match args.get(1) {
                    Some(v) if v.is_undefined() => n,
                    Some(v) => {
                        let mut e = to_i64(v);
                        if e < 0 {
                            e = (n + e).max(0);
                        }
                        e.min(n)
                    }
                    None => n,
                };
                let mut out: Vec<Value> = Vec::new();
                let mut i = start;
                while i < end {
                    out.push(ad.get(i as usize));
                    i += 1;
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "concat" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut out = ad.borrow().to_values();
                for a in args {
                    if let Some(other) = a.as_array() {
                        out.extend(other.borrow().to_values());
                    } else {
                        out.push(a.clone());
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "indexOf" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let needle = args.first().cloned().unwrap_or(Value::undefined());
                let from = args.get(1).map(|v| {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }).unwrap_or(0);
                let n = ad.len() as i64;
                let mut i = if from < 0 { (n + from).max(0) } else { from.min(n) };
                while i < n {
                    if strict_equal(&ad.get(i as usize), &needle) {
                        return Value::int(i);
                    }
                    i += 1;
                }
                Value::int(-1)
            } else {
                Value::undefined()
            }
        })),
        "includes" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let needle = args.first().cloned().unwrap_or(Value::undefined());
                let from = args.get(1).map(|v| {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }).unwrap_or(0);
                let n = ad.len() as i64;
                let mut i = if from < 0 { (n + from).max(0) } else { from.min(n) };
                let needle_nan = needle.is_number() && needle.to_number().is_nan();
                while i < n {
                    let e = ad.get(i as usize);
                    if needle_nan {
                        if e.is_number() && e.to_number().is_nan() {
                            return Value::bool(true);
                        }
                    } else if strict_equal(&e, &needle) {
                        return Value::bool(true);
                    }
                    i += 1;
                }
                Value::bool(false)
            } else {
                Value::undefined()
            }
        })),
        "map" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out: Vec<Value> = Vec::with_capacity(ad.len());
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    out.push(vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "forEach" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                }
            }
            Value::undefined()
        })),
        "find" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return e.clone();
                    }
                }
            }
            Value::undefined()
        })),
        "findIndex" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::int(i as i64);
                    }
                }
            }
            Value::int(-1)
        })),
        "filter" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut out: Vec<Value> = Vec::new();
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        out.push(e.clone());
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "some" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::bool(true);
                    }
                }
            }
            Value::bool(false)
        })),
        "every" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if !r.is_truthy() {
                        return Value::bool(false);
                    }
                }
            }
            Value::bool(true)
        })),
        "reverse" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                match &mut *guard {
                    ArrayData::Ints(vs) => vs.reverse(),
                    ArrayData::Values(vs) => vs.reverse(),
                }
            }
            // JS returns the same (mutated) array.
            arr.clone()
        })),
        "sort" => Value::native(Arc::new(move |args, vm| {
            let cmp = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let mut vals = guard.to_values();
                // ES: undefined always sorts to the end, even with a
                // comparator.
                let mut undefs = 0usize;
                vals.retain(|v| {
                    if v.is_undefined() {
                        undefs += 1;
                        false
                    } else {
                        true
                    }
                });
                if cmp.is_undefined() {
                    // Default order: ToString each element once, compare
                    // lexicographically by code unit — `[10, 9]` sorts as
                    // `[10, 9]` ("10" < "9"), matching Node/V8.
                    let mut pairs: Vec<(String, Value)> =
                        vals.into_iter().map(|v| (to_string_js(&v), v)).collect();
                    stable_merge_sort(&mut pairs, |a, b| a.0.cmp(&b.0));
                    vals = pairs.into_iter().map(|(_, v)| v).collect();
                } else {
                    let cmp2 = cmp.clone();
                    stable_merge_sort(&mut vals, |a, b| {
                        let r = vm.call_value(&cmp2, &[a.clone(), b.clone()]);
                        let n = r.to_number();
                        if n < 0.0 {
                            std::cmp::Ordering::Less
                        } else if n > 0.0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    });
                }
                for _ in 0..undefs {
                    vals.push(Value::undefined());
                }
                // Write back into the same box; int-only results stay packed.
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
            }
            arr.clone()
        })),
        // `reduce(cb, initial?)`: left-to-right accumulator fold. No initial
        // value uses the first element as the accumulator and starts at 1.
        "reduce" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut start = 0usize;
                let mut acc = args.get(1).cloned().unwrap_or(Value::undefined());
                if args.get(1).is_none() {
                    if vals.is_empty() {
                        vm.throw_exception(Value::string(
                            "TypeError: Reduce of empty array with no initial value".to_string(),
                        ));
                        return Value::undefined();
                    }
                    acc = vals[0].clone();
                    start = 1;
                }
                for i in start..vals.len() {
                    acc = vm.call_value(&cb, &[
                        acc,
                        vals[i].clone(),
                        Value::int(i as i64),
                        arr.clone(),
                    ]);
                }
                acc
            } else {
                Value::undefined()
            }
        })),
        // `reduceRight(cb, initial?)`: right-to-left fold (indexes still pass
        // as the element's own index).
        "reduceRight" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let n = vals.len();
                let mut acc = args.get(1).cloned().unwrap_or(Value::undefined());
                if args.get(1).is_none() {
                    if n == 0 {
                        vm.throw_exception(Value::string(
                            "TypeError: Reduce of empty array with no initial value".to_string(),
                        ));
                        return Value::undefined();
                    }
                    acc = vals[n - 1].clone();
                }
                let last = if args.get(1).is_none() { n - 1 } else { n };
                for i in (0..last).rev() {
                    acc = vm.call_value(&cb, &[
                        acc,
                        vals[i].clone(),
                        Value::int(i as i64),
                        arr.clone(),
                    ]);
                }
                acc
            } else {
                Value::undefined()
            }
        })),
        // `splice(start, deleteCount, ...items)`: remove `deleteCount`
        // elements from `start` and insert `items` in their place; returns the
        // removed elements as a new array.
        "splice" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let mut vals = guard.to_values();
                let n = vals.len() as i64;
                let to_i64 = |v: &Value| {
                    let x = v.to_number();
                    if x.is_nan() { 0 } else { x.trunc() as i64 }
                };
                let start = match args.first() {
                    Some(v) if v.is_undefined() => 0i64,
                    Some(v) => {
                        let mut s = to_i64(v);
                        if s < 0 { s = (n + s).max(0); }
                        s.min(n)
                    }
                    None => 0,
                };
                let del = match args.get(1) {
                    Some(v) if v.is_undefined() => (n - start).max(0),
                    Some(v) => to_i64(v).clamp(0, (n - start).max(0)),
                    None => (n - start).max(0),
                };
                let items: Vec<Value> = args.iter().skip(2).cloned().collect();
                let removed: Vec<Value> = vals.drain(start as usize..(start + del) as usize).collect();
                for (j, it) in items.iter().enumerate() {
                    vals.insert((start + j as i64) as usize, it.clone());
                }
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
                Value::array(removed)
            } else {
                Value::undefined()
            }
        })),
        // `flat(depth?)`: recursively flatten nested arrays up to `depth`
        // (default 1); every level deeper than depth stays as-is.
        "flat" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let depth = match args.first() {
                    Some(v) if v.is_undefined() => 1usize,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() { 0 } else if x.is_infinite() { if x > 0.0 { usize::MAX } else { 0 } } else { x.trunc().max(0.0) as usize }
                    }
                    None => 1,
                };
                let mut out: Vec<Value> = Vec::new();
                fn push_flat(src: &Value, depth: usize, out: &mut Vec<Value>) {
                    if depth > 0 {
                        if let Some(inner) = src.as_array() {
                            let inner = inner.borrow();
                            for e in inner.to_values() {
                                push_flat(&e, depth - 1, out);
                            }
                            return;
                        }
                    }
                    out.push(src.clone());
                }
                for e in ad.to_values() {
                    push_flat(&e, depth, &mut out);
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `flatMap(cb)`: map then flat(1).
        "flatMap" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut out: Vec<Value> = Vec::new();
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if let Some(inner) = r.as_array() {
                        out.extend(inner.borrow().to_values());
                    } else {
                        out.push(r);
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `at(i)`: index, negative counts from the end; out of bounds is
        // undefined (unlike `[]` which returns undefined too but never
        // negative-clamps).
        "at" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let n = ad.len() as i64;
                let i = match args.first() {
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() { 0 } else { x.trunc() as i64 }
                    }
                    None => 0,
                };
                let idx = if i < 0 { n + i } else { i };
                if idx < 0 || idx >= n {
                    Value::undefined()
                } else {
                    ad.get(idx as usize)
                }
            } else {
                Value::undefined()
            }
        })),
        // `fill(value, start?, end?)`: write `value` into [start, end)
        // (negative counts from the end), returns the same array.
        "fill" => Value::native(Arc::new(move |args, _vm| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let n = guard.len() as i64;
                let arg = |i: usize, dflt: i64| -> i64 {
                    match args.get(i) {
                        Some(v) if v.is_undefined() => dflt,
                        Some(v) => {
                            let x = v.to_number();
                            if x.is_nan() { 0 } else { x.trunc() as i64 }
                        }
                        None => dflt,
                    }
                };
                let mut a = arg(1, 0);
                let mut b = arg(2, n);
                if a < 0 { a = (n + a).max(0); }
                if b < 0 { b = (n + b).max(0); }
                a = a.min(n);
                b = b.min(n);
                let mut vals = guard.to_values();
                for i in a..b {
                    vals[i as usize] = value.clone();
                }
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
            }
            arr.clone()
        })),
        // `copyWithin(target, start, end?)`: copy the slice [start, end) to
        // `target` (negative counts from the end), returning the same array.
        "copyWithin" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let n = guard.len() as i64;
                let arg = |i: usize, dflt: i64| -> i64 {
                    match args.get(i) {
                        Some(v) if v.is_undefined() => dflt,
                        Some(v) => {
                            let x = v.to_number();
                            if x.is_nan() { 0 } else { x.trunc() as i64 }
                        }
                        None => dflt,
                    }
                };
                let mut t = arg(0, 0);
                let mut a = arg(1, 0);
                let mut b = arg(2, n);
                if t < 0 { t = (n + t).max(0); }
                if a < 0 { a = (n + a).max(0); }
                if b < 0 { b = (n + b).max(0); }
                t = t.min(n);
                a = a.min(n);
                b = b.min(n);
                if a < b && t < n {
                    let mut vals = guard.to_values();
                    let count = (b - a).min(n - t);
                    // Memmove semantics: a forward copy when target < start
                    // would clobber the source, so copy in the safe direction.
                    if t <= a {
                        for k in 0..count {
                            vals[(t + k) as usize] = vals[(a + k) as usize].clone();
                        }
                    } else {
                        for k in (0..count).rev() {
                            vals[(t + k) as usize] = vals[(a + k) as usize].clone();
                        }
                    }
                    let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                        ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                    } else {
                        ArrayData::Values(vals)
                    };
                    *guard = new_data;
                }
            }
            arr.clone()
        })),
        // `keys()` / `values()` / `entries()` — array snapshots (the engine's
        // container convention; see Map/Set) of indices, elements, and
        // [index, element] pairs.
        "keys" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out = Vec::new();
                for i in 0..ad.len() {
                    out.push(Value::int(i as i64));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "values" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                Value::array(ad.borrow().to_values())
            } else {
                Value::undefined()
            }
        })),
        "entries" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out = Vec::new();
                for (i, e) in ad.to_values().iter().enumerate() {
                    out.push(Value::array(vec![Value::int(i as i64), e.clone()]));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `findLast` / `findLastIndex`: like find/findIndex, scanning from the
        // end.
        "findLast" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for i in (0..vals.len()).rev() {
                    let r = vm.call_value(&cb, &[vals[i].clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return vals[i].clone();
                    }
                }
            }
            Value::undefined()
        })),
        "findLastIndex" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for i in (0..vals.len()).rev() {
                    let r = vm.call_value(&cb, &[vals[i].clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::int(i as i64);
                    }
                }
            }
            Value::int(-1)
        })),
        _ => Value::undefined(),
    }
}

/// `String.prototype` method reads for `s.name` — natives capturing the
/// string. Semantics follow JS: `charAt` returns the char at the index ("" out
/// of bounds; negative counts from the end), `substring` clamps negatives to
/// zero and swaps inverted bounds, `split` on "" splits into characters, on an
/// undefined separator returns `[s]`, `toUpperCase` maps through Unicode
/// uppercase (ASCII-exact for the engine's corpus).
fn string_prop(obj: &Value, name: &str) -> Value {
    let s = obj.clone();
    match name {
        // JS length counts UTF-16 code units ("😀".length is 2), not code points.
        "length" => Value::int(
            s.as_str()
                .map(|s| s.encode_utf16().count() as i64)
                .unwrap_or(0),
        ),
        "charAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            // JS charAt: NaN → 0, and anything out of [0, len) is "" —
            // negatives do NOT count from the end (that's `at`).
            if i < 0 || i >= n {
                Value::string(String::new())
            } else {
                Value::string(chars[i as usize].to_string())
            }
        })),
        "substring" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let n = s.chars().count() as i64;
            let clamp = |v: Option<&Value>, dflt: i64| -> i64 {
                match v {
                    Some(v) if v.is_undefined() => dflt,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else {
                            x.trunc().clamp(0.0, n as f64) as i64
                        }
                    }
                    None => dflt,
                }
            };
            let mut a = clamp(args.first(), 0);
            let mut b = clamp(args.get(1), n);
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let out: String = s.chars().skip(a as usize).take((b - a) as usize).collect();
            Value::string(out)
        })),
        "split" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            // Regex separator: split on every match, including participating
            // capture groups in the result (JS semantics), empty matches
            // split between chars.
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                // Empty input: an empty-matching regex yields [] (Node), a
                // non-matching one [""] (the single empty part).
                if hay.is_empty() {
                    let matched = regex::search(&prog, &hay, 0).is_some();
                    return Value::array(if matched {
                        Vec::new()
                    } else {
                        vec![Value::string(String::new())]
                    });
                }
                let mut parts: Vec<Value> = Vec::new();
                let mut pos = 0usize;
                let mut terminal_empty = false;
                for (a, b, caps) in regex::scan_all(&prog, &hay) {
                    if b > a {
                        // Non-empty separator: always push the pre-segment
                        // (even "" when the separator sits at position 0,
                        // like `"ab".split(/(a)/)` -> ["", "a", "b"]).
                        parts.push(Value::string(hay[pos..a].iter().collect()));
                        let n_groups = (caps.len().saturating_sub(1)) / 2;
                        for g in 1..=n_groups {
                            if let Some((x, y)) = caps.get(g).and_then(|c| *c) {
                                parts.push(Value::string(hay[x..y].iter().collect()));
                            }
                        }
                        pos = b;
                    } else if a < hay.len() {
                        // Empty separator between chars: the char itself
                        // becomes the part ("abc".split(/(?:)/) -> ["a",
                        // "b", "c"]).
                        parts.push(Value::string(hay[a..a + 1].iter().collect()));
                        pos = a + 1;
                    } else {
                        // Terminal empty match: nothing after it, and no
                        // trailing empty part (Node: "abc".split(/(?:)/)
                        // ends at "c").
                        terminal_empty = true;
                        break;
                    }
                }
                if !terminal_empty {
                    parts.push(Value::string(hay[pos..].iter().collect()));
                }
                return Value::array(parts);
            }
            let parts: Vec<Value> = match args.first() {
                None => vec![Value::string(s.to_string())],
                Some(v) if v.is_undefined() => vec![Value::string(s.to_string())],
                Some(v) if v.as_str() == Some("") => {
                    s.chars().map(|c| Value::string(c.to_string())).collect()
                }
                Some(v) => match v.as_str() {
                    Some(sep) if sep.is_empty() => {
                        s.chars().map(|c| Value::string(c.to_string())).collect()
                    }
                    Some(sep) => s.split(sep).map(|p| Value::string(p.to_string())).collect(),
                    // Non-string separator: coerce like JS ToString.
                    None => s
                        .split(&to_string_js(v))
                        .map(|p| Value::string(p.to_string()))
                        .collect(),
                },
            };
            Value::array(parts)
        })),
        "toUpperCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_uppercase())
        })),
        "trim" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(js_trim(s.as_str().unwrap_or("")).to_string())
        })),
        "slice" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let arg = |i: usize, dflt: i64| -> i64 {
                match args.get(i) {
                    Some(v) if v.is_undefined() => dflt,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else if x.is_infinite() {
                            if x > 0.0 {
                                n
                            } else {
                                0
                            }
                        } else {
                            x.trunc() as i64
                        }
                    }
                    None => dflt,
                }
            };
            let mut a = arg(0, 0);
            let mut b = arg(1, n);
            if a < 0 {
                a = (n + a).max(0);
            }
            if b < 0 {
                b = (n + b).max(0);
            }
            a = a.min(n);
            b = b.min(n);
            if a > b {
                Value::string(String::new())
            } else {
                let out: String = chars[a as usize..b as usize].iter().collect();
                Value::string(out)
            }
        })),
        "substr" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let start = match args.first() {
                Some(v) if v.is_undefined() => 0i64,
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else if x.is_infinite() {
                        if x > 0.0 {
                            n
                        } else {
                            0
                        }
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let start = if start < 0 {
                (n + start).max(0)
            } else {
                start.min(n)
            };
            let mut len = match args.get(1) {
                Some(v) if v.is_undefined() => n - start,
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else if x.is_infinite() {
                        if x > 0.0 {
                            n - start
                        } else {
                            return Value::string(String::new());
                        }
                    } else {
                        x.trunc() as i64
                    }
                }
                None => n - start,
            };
            if len <= 0 {
                return Value::string(String::new());
            }
            len = len.min(n - start);
            let out: String = chars[start as usize..(start + len) as usize].iter().collect();
            Value::string(out)
        })),
        "includes" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let pos = if num_pos.is_nan() {
                0.0
            } else {
                num_pos.trunc()
            };
            // +∞ fromIndex → false (ES); empty search otherwise always true.
            if pos.is_infinite() && pos > 0.0 {
                return Value::bool(false);
            }
            let start = (pos.max(0.0) as i64).min(hay.len() as i64) as usize;
            if needle.is_empty() {
                return Value::bool(true);
            }
            Value::bool(char_index_of(&hay, &needle, start).is_some())
        })),
        "startsWith" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let pos = if num_pos.is_nan() {
                0.0
            } else {
                num_pos.trunc()
            };
            let start = if pos.is_infinite() {
                if pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (pos.max(0.0) as i64).min(hay.len() as i64) as usize
            };
            if needle.is_empty() {
                return Value::bool(true);
            }
            if start + needle.len() > hay.len() {
                return Value::bool(false);
            }
            Value::bool(hay[start..start + needle.len()] == needle[..])
        })),
        "endsWith" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::INFINITY,
                Some(v) => v.to_number(),
                None => f64::INFINITY,
            };
            let pos = if num_pos.is_nan() {
                // Explicit NaN → 0 (ToIntegerOrInfinity); undefined → +∞.
                0.0
            } else {
                num_pos.trunc()
            };
            let end = if pos.is_infinite() {
                if pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (pos.max(0.0) as i64).min(hay.len() as i64) as usize
            };
            if needle.is_empty() {
                return Value::bool(true);
            }
            if needle.len() > end {
                return Value::bool(false);
            }
            Value::bool(hay[end - needle.len()..end] == needle[..])
        })),
        "padStart" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let len = s.chars().count() as i64;
            let target = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() || x <= 0.0 {
                        0
                    } else if x.is_infinite() {
                        // ToLength(+∞) → 2^53-1 (practically: saturate)
                        i64::MAX / 2
                    } else {
                        x.floor() as i64
                    }
                }
                None => 0,
            };
            let needed = target - len;
            if needed <= 0 {
                return Value::string(s.to_string());
            }
            let pad: String = match args.get(1) {
                Some(v) => to_string_js(v),
                None => " ".to_string(),
            };
            if pad.is_empty() {
                return Value::string(s.to_string());
            }
            let fill: String = pad.chars().cycle().take(needed as usize).collect();
            Value::string(format!("{}{}", fill, s))
        })),
        "padEnd" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let len = s.chars().count() as i64;
            let target = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() || x <= 0.0 {
                        0
                    } else if x.is_infinite() {
                        i64::MAX / 2
                    } else {
                        x.floor() as i64
                    }
                }
                None => 0,
            };
            let needed = target - len;
            if needed <= 0 {
                return Value::string(s.to_string());
            }
            let pad: String = match args.get(1) {
                Some(v) => to_string_js(v),
                None => " ".to_string(),
            };
            if pad.is_empty() {
                return Value::string(s.to_string());
            }
            let fill: String = pad.chars().cycle().take(needed as usize).collect();
            Value::string(format!("{}{}", s, fill))
        })),
        "indexOf" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            // ES String.prototype.indexOf: pos = ToIntegerOrInfinity(position)
            // (NaN/undefined → 0); +∞ → len, -∞ → 0, then clamp to [0, len].
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let start = if num_pos.is_nan() {
                0
            } else if num_pos.is_infinite() {
                if num_pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (num_pos.trunc().max(0.0) as i64).min(hay.len() as i64) as usize
            };
            match char_index_of(&hay, &needle, start) {
                Some(i) => Value::int(i as i64),
                None => Value::int(-1),
            }
        })),
        "lastIndexOf" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let n = hay.len();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            // ES String.prototype.lastIndexOf: NaN position → +∞ → len;
            // -∞ → -1; finite values truncate toward zero.
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let m = needle.len();
            if m == 0 {
                // Empty needle: min(pos, len) — but -∞ (or negative) is -1 in
                // Node? No: `"abc".lastIndexOf("", -1)` → 0 (spec: min(pos, len)
                // after pos is clamped by the caller's +∞/finite rules).
                let pos = if num_pos.is_nan() {
                    n
                } else if num_pos.is_infinite() {
                    if num_pos > 0.0 {
                        n
                    } else {
                        0
                    }
                } else {
                    (num_pos.trunc().max(0.0) as i64).min(n as i64) as usize
                };
                return Value::int(pos as i64);
            }
            let pos: i64 = if num_pos.is_nan() {
                n as i64
            } else if num_pos.is_infinite() {
                if num_pos > 0.0 {
                    n as i64
                } else {
                    return Value::int(-1);
                }
            } else {
                num_pos.trunc() as i64
            };
            // Clamp only downward: "If pos + searchLen > len, set pos to
            // len - searchLen" — the search stays at-or-before the original
            // fromIndex (lastIndexOf("b", 3) on "abcabc" → 1, not 4).
            let start = if pos + m as i64 > n as i64 {
                (n - m) as i64
            } else {
                pos
            };
            let start = start.max(0) as usize;
            match char_last_index_of(&hay, &needle, start) {
                Some(i) => Value::int(i as i64),
                None => Value::int(-1),
            }
        })),
        "match" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("").to_string();
            let hay: Vec<char> = s.chars().collect();
            let re = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(st) = re.as_regex() {
                let st = st.clone();
                let global = st
                    .lock()
                    .unwrap_or_else(|g| g.into_inner())
                    .compiled
                    .flags
                    .global;
                if global {
                    // Array of all full-match texts (no captures/index/input),
                    // like Node.
                    let prog = st
                        .lock()
                        .unwrap_or_else(|g| g.into_inner())
                        .compiled
                        .clone();
                    let mut texts = Vec::new();
                    for (a, b, _) in regex::scan_all(&prog, &hay) {
                        texts.push(Value::string(hay[a..b].iter().collect()));
                    }
                    Value::array(texts)
                } else {
                    // Exec-like: match object or null. lastIndex is ignored
                    // for non-global match (Node behavior).
                    regex_exec_value(&st, &Value::string(s))
                }
            } else {
                // String pattern: coerced to a non-global regex (Node).
                match regex::compile_from_str(&to_string_js(&re), "") {
                    Ok(prog) => {
                        let st = Arc::new(Mutex::new(RegexState {
                            compiled: Arc::new(prog),
                            last_index: 0,
                        }));
                        regex_exec_value(&st, &Value::string(s))
                    }
                    Err(_) => Value::null(),
                }
            }
        })),
        "search" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let re = args.first().cloned().unwrap_or(Value::undefined());
            // Node ignores lastIndex for search; a string arg is coerced to
            // a non-global regex.
            let prog: Arc<RegexCompiled> = if let Some(st) = re.as_regex() {
                st.lock()
                    .unwrap_or_else(|g| g.into_inner())
                    .compiled
                    .clone()
            } else {
                match regex::compile_from_str(&to_string_js(&re), "") {
                    Ok(p) => Arc::new(p),
                    Err(_) => return Value::int(-1),
                }
            };
            match regex::search(&prog, &hay, 0) {
                Some(m) => Value::int(regex::char_pos_to_utf16(&hay, m.start) as i64),
                None => Value::int(-1),
            }
        })),
        "replace" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let repl = args.get(1).cloned().unwrap_or(Value::undefined());
            let hay: Vec<char> = s.chars().collect();
            // Regex search arg: full semantics (global -> all matches, else
            // first only; captures fed to the replacer function or `$n`).
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                let global = prog.flags.global;
                let matches: Vec<regex::Match> = if global {
                    let mut ms = Vec::new();
                    for (a, b, caps) in regex::scan_all(&prog, &hay) {
                        ms.push(regex::Match { start: a, end: b, caps });
                    }
                    ms
                } else {
                    regex::search(&prog, &hay, 0)
                        .into_iter()
                        .collect::<Vec<_>>()
                };
                let mut out = String::new();
                let mut pos = 0usize;
                let hay_text = s.to_string();
                for m in &matches {
                    let a = m.start;
                    let b = m.end;
                    out.push_str(&hay[pos..a].iter().collect::<String>());
                    let matched: String = hay[a..b].iter().collect();
                    let r = if let Some(f) = repl.as_function() {
                        // Replacer: (match, ...captures, offset, string).
                        let n_groups = (m.caps.len().saturating_sub(1)) / 2;
                        let mut fargs: Vec<Value> = Vec::with_capacity(n_groups + 3);
                        fargs.push(Value::string(matched.clone()));
                        for g in 1..=n_groups {
                            match m.caps.get(g).and_then(|c| *c) {
                                Some((x, y)) => {
                                    fargs.push(Value::string(hay[x..y].iter().collect()))
                                }
                                None => fargs.push(Value::undefined()),
                            }
                        }
                        fargs.push(Value::int(regex::char_pos_to_utf16(&hay, a) as i64));
                        fargs.push(Value::string(hay_text.clone()));
                        to_string_js(&vm.call_value(&Value::function(f.clone()), &fargs))
                    } else {
                        let before_str: String = hay[..a].iter().collect();
                        let after_str: String = hay[b..].iter().collect();
                        expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before_str,
                            &after_str,
                            &m.caps,
                            &hay,
                        )
                    };
                    out.push_str(&r);
                    if b > a {
                        pos = b;
                    } else {
                        // Empty match: advance one char like the scanner.
                        pos = (a + 1).min(hay.len());
                    }
                }
                out.push_str(&hay[pos..].iter().collect::<String>());
                return Value::string(out);
            }
            // String search arg: literal first-occurrence replace (Node
            // replaces only the first occurrence for a string pattern).
            let search: String = match args.first() {
                Some(v) => to_string_js(v),
                None => String::new(),
            };
            let needle: Vec<char> = search.chars().collect();
            match char_index_of(&hay, &needle, 0) {
                None => Value::string(s.to_string()),
                Some(i) => {
                    let before: String = hay[..i].iter().collect();
                    let after: String = hay[i + needle.len()..].iter().collect();
                    let matched: String = hay[i..i + needle.len()].iter().collect();
                    if let Some(f) = repl.as_function() {
                        // No capture groups without regex: (match, offset, string).
                        let out = vm.call_value(
                            &Value::function(f.clone()),
                            &[
                                Value::string(matched.clone()),
                                Value::int(i as i64),
                                Value::string(s.to_string()),
                            ],
                        );
                        let r = to_string_js(&out);
                        Value::string(format!("{}{}{}", before, r, after))
                    } else {
                        let r = expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before,
                            &after,
                            &[],
                            &[],
                        );
                        Value::string(format!("{}{}{}", before, r, after))
                    }
                }
            }
        })),
        "charCodeAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let units: Vec<u16> = s.encode_utf16().collect();
            if i < 0 || i as usize >= units.len() {
                Value::number(f64::NAN)
            } else {
                Value::int(units[i as usize] as i64)
            }
        })),
        "codePointAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let units: Vec<u16> = s.encode_utf16().collect();
            if i < 0 || i as usize >= units.len() {
                return Value::undefined();
            }
            let u = units[i as usize];
            let cp: u32 = if u >= 0xD800
                && u <= 0xDBFF
                && (i as usize) + 1 < units.len()
            {
                let lo = units[i as usize + 1];
                if lo >= 0xDC00 && lo <= 0xDFFF {
                    0x10000 + ((u - 0xD800) as u32) * 0x400 + (lo - 0xDC00) as u32
                } else {
                    u as u32
                }
            } else {
                u as u32
            };
            Value::int(cp as i64)
        })),
        // `toLowerCase` / `toLocaleLowerCase` — the missing counterpart to
        // `toUpperCase` (Unicode lowercase, ASCII-exact for the corpus).
        "toLowerCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_lowercase())
        })),
        "toLocaleLowerCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_lowercase())
        })),
        "toLocaleUpperCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_uppercase())
        })),
        // `repeat(n)`: the string repeated n times; negative/fractional n is
        // a RangeError, NaN/0 → "".
        "repeat" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let n = match args.first() {
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            if n.is_nan() || n == 0.0 {
                return Value::string(String::new());
            }
            if n < 0.0 || n.is_infinite() {
                vm.throw_exception(Value::string(
                    "RangeError: Invalid count value".to_string(),
                ));
                return Value::undefined();
            }
            let count = n.floor() as usize;
            if s.len() * count > (1usize << 28) {
                vm.throw_exception(Value::string(
                    "RangeError: Invalid string length".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(s.repeat(count))
        })),
        // `at(i)`: UTF-16 code-unit index, negative counts from the end.
        "at" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let units: Vec<u16> = s.encode_utf16().collect();
            let n = units.len() as i64;
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() { 0 } else { x.trunc() as i64 }
                }
                None => 0,
            };
            let idx = if i < 0 { n + i } else { i };
            if idx < 0 || idx as usize >= units.len() {
                return Value::undefined();
            }
            // Return the code point that starts at this unit: a lone high
            // surrogate is emitted on its own (JS String.raw behavior via
            // String.fromCharCode semantics).
            let cp = units[idx as usize];
            if cp >= 0xD800 && cp <= 0xDBFF && (idx as usize) + 1 < units.len() {
                let lo = units[idx as usize + 1];
                if lo >= 0xDC00 && lo <= 0xDFFF {
                    let full = 0x10000 + ((cp - 0xD800) as u32) * 0x400 + (lo - 0xDC00) as u32;
                    if let Some(c) = char::from_u32(full) {
                        return Value::char_str_utf8(c);
                    }
                }
            }
            if let Some(c) = char::from_u32(cp as u32) {
                Value::char_str_utf8(c)
            } else {
                Value::string(String::new())
            }
        })),
        // `replaceAll(search, repl)`: like replace but replaces EVERY
        // occurrence; a non-global regex search is a TypeError.
        "replaceAll" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let repl = args.get(1).cloned().unwrap_or(Value::undefined());
            // Regex search: must be /g, else TypeError (Node).
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let g = st.lock().unwrap_or_else(|g| g.into_inner());
                if !g.compiled.flags.global {
                    vm.throw_exception(Value::string(format!(
                        "TypeError: String.prototype.replaceAll called with a non-global RegExp argument"
                    )));
                    return Value::undefined();
                }
                drop(g);
                // Reuse the replace path with a global scan.
                let hay: Vec<char> = s.chars().collect();
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                let matches: Vec<regex::Match> = {
                    let mut ms = Vec::new();
                    for (a, b, caps) in regex::scan_all(&prog, &hay) {
                        ms.push(regex::Match { start: a, end: b, caps });
                    }
                    ms
                };
                let mut out = String::new();
                let mut pos = 0usize;
                let hay_text = s.to_string();
                for m in &matches {
                    let a = m.start;
                    let b = m.end;
                    out.push_str(&hay[pos..a].iter().collect::<String>());
                    let matched: String = hay[a..b].iter().collect();
                    let r = if let Some(f) = repl.as_function() {
                        let n_groups = (m.caps.len().saturating_sub(1)) / 2;
                        let mut fargs: Vec<Value> = Vec::with_capacity(n_groups + 3);
                        fargs.push(Value::string(matched.clone()));
                        for g in 1..=n_groups {
                            match m.caps.get(g).and_then(|c| *c) {
                                Some((x, y)) => {
                                    fargs.push(Value::string(hay[x..y].iter().collect()))
                                }
                                None => fargs.push(Value::undefined()),
                            }
                        }
                        fargs.push(Value::int(regex::char_pos_to_utf16(&hay, a) as i64));
                        fargs.push(Value::string(hay_text.clone()));
                        to_string_js(&vm.call_value(&Value::function(f.clone()), &fargs))
                    } else {
                        let before_str: String = hay[..a].iter().collect();
                        let after_str: String = hay[b..].iter().collect();
                        expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before_str,
                            &after_str,
                            &m.caps,
                            &hay,
                        )
                    };
                    out.push_str(&r);
                    if b > a {
                        pos = b;
                    } else {
                        pos = (a + 1).min(hay.len());
                    }
                }
                out.push_str(&hay[pos..].iter().collect::<String>());
                return Value::string(out);
            }
            // String search: replace every non-overlapping occurrence.
            let search: String = match args.first() {
                Some(v) => to_string_js(v),
                None => String::new(),
            };
            if search.is_empty() {
                // Empty needle: insert between every char (and at both ends).
                let r = to_string_js(&repl);
                let chars: Vec<char> = s.chars().collect();
                let mut out = String::new();
                for (i, c) in chars.iter().enumerate() {
                    out.push_str(&r);
                    out.push(*c);
                    if i == chars.len() - 1 {
                        out.push_str(&r);
                    }
                }
                if chars.is_empty() {
                    return Value::string(r);
                }
                return Value::string(out);
            }
            let needle: Vec<char> = search.chars().collect();
            let hay: Vec<char> = s.chars().collect();
            let mut out = String::new();
            let mut pos = 0usize;
            while pos <= hay.len() {
                match char_index_of(&hay, &needle, pos) {
                    Some(i) => {
                        out.push_str(&hay[pos..i].iter().collect::<String>());
                        let matched: String = hay[i..i + needle.len()].iter().collect();
                        if let Some(f) = repl.as_function() {
                            let r = vm.call_value(
                                &Value::function(f.clone()),
                                &[
                                    Value::string(matched.clone()),
                                    Value::int(i as i64),
                                    Value::string(s.to_string()),
                                ],
                            );
                            out.push_str(&to_string_js(&r));
                        } else {
                            let before_str: String = hay[..i].iter().collect();
                            let after_str: String = hay[i + needle.len()..].iter().collect();
                            out.push_str(&expand_replacement(
                                &to_string_js(&repl),
                                &matched,
                                &before_str,
                                &after_str,
                                &[],
                                &[],
                            ));
                        }
                        pos = i + needle.len();
                    }
                    None => {
                        out.push_str(&hay[pos..].iter().collect::<String>());
                        break;
                    }
                }
            }
            Value::string(out)
        })),
        _ => Value::undefined(),
    }
}

fn make_print_fn(sink: Option<Arc<Mutex<Vec<String>>>>) -> Value {
    Value::native(Arc::new(move |args, _vm| {
        let parts: Vec<String> = args
            .iter()
            .map(|arg| match arg.as_str() {
                Some(s) => s.to_string(),
                None => format!("{}", arg),
            })
            .collect();
        let line = parts.join(" ");
        match &sink {
            Some(s) => {
                s.lock().unwrap().push(line);
            }
            None => println!("{}", line),
        }
        Value::undefined()
    }))
}

fn make_http_module() -> Value {
    let create_server = Value::native(Arc::new(|args, _vm| {
        let handler = args.first().cloned().unwrap_or(Value::undefined());
        let listen = Value::native(Arc::new(move |listen_args, vm| {
            let port = listen_args.first().map(|v| v.to_number()).unwrap_or(0.0) as u16;
            serve_http(vm, &handler, port);
            Value::undefined()
        }));
        let mut srv = HashMap::new();
        srv.insert("listen".to_string(), listen);
        Value::object(srv)
    }));
    let mut m = HashMap::new();
    m.insert("createServer".to_string(), create_server);
    Value::object(m)
}

/// The zero-copy polyglot bridge: typed-array allocation and scalar read/write
/// over one shared segment. A sidecar process (Python via ctypes, another
/// binary) that knows the segment's base address reads exactly the bytes the
/// JS runtime writes — no serialization, no copy.
fn make_memory_module(shared: Arc<SidecarMemory>) -> Value {
    let base = shared.raw_ptr() as usize;
    // offset_of: turn a JS-visible `buf.ptr` back into a segment offset.
    let offset_of = move |p: f64| -> Option<usize> {
        let p = p as usize;
        if p >= base {
            Some(p - base)
        } else {
            None
        }
    };

    let make_alloc = |shared: &Arc<SidecarMemory>,
                      f: fn(&SidecarMemory, &[f32]) -> Result<usize, SharedMemoryError>|
     -> Value {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<f32> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number() as f32)
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as f32]
            } else if let Some(n) = args.first().and_then(|v| v.as_number()) {
                vec![n as f32]
            } else {
                Vec::new()
            };
            match f(&shared, &vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let allocate_f32 = make_alloc(&shared, SidecarMemory::allocate_float32_array);
    let allocate_f64 = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<f64> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number())
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_number()) {
                vec![n]
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as f64]
            } else {
                Vec::new()
            };
            match shared.allocate_float64_array(&vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let allocate_i32 = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<i32> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number() as i32)
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as i32]
            } else {
                Vec::new()
            };
            match shared.allocate_int32_array(&vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let alloc_bytes = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let n = args.first().map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
            match shared.bump(n) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, n)
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };

    // Scalar read/write: `(buf.ptr, element_index_or_byte_offset)`.
    let make_scalar = |shared: Arc<SidecarMemory>,
                       read: fn(&SidecarMemory, usize) -> Result<f64, SharedMemoryError>,
                       write: fn(&SidecarMemory, usize, f64) -> Result<(), SharedMemoryError>| {
        let read = Value::native({
            let shared = shared.clone();
            Arc::new(move |args, _vm| {
                let p = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                let off = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
                match offset_of(p).and_then(|o| read(&shared, o + off).ok()) {
                    Some(v) => Value::number(v),
                    None => Value::undefined(),
                }
            })
        });
        let write = Value::native({
            let shared = shared.clone();
            Arc::new(move |args, _vm| {
                let p = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                let off = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
                let v = args.get(2).map(|x| x.to_number()).unwrap_or(f64::NAN);
                let ok = match offset_of(p) {
                    Some(o) => write(&shared, o + off, v).is_ok(),
                    None => false,
                };
                Value::bool(ok)
            })
        });
        (read, write)
    };
    let (read_f32, write_f32) = make_scalar(
        shared.clone(),
        |m, o| m.read_float32(o).map(|v| v as f64),
        |m, o, v| m.write_float32(o, v as f32),
    );
    let (read_f64, write_f64) = make_scalar(
        shared.clone(),
        |m, o| m.read_float64(o),
        |m, o, v| m.write_float64(o, v),
    );
    let (read_i32, write_i32) = make_scalar(
        shared.clone(),
        |m, o| m.read_int32(o).map(|v| v as f64),
        |m, o, v| m.write_int32(o, v as i32),
    );
    let (read_u8, write_u8) = make_scalar(
        shared.clone(),
        |m, o| m.read_uint8(o).map(|v| v as f64),
        |m, o, v| m.write_uint8(o, v as u8),
    );

    let size = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| Value::int(shared.capacity() as i64)))
    };
    let used = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| Value::int(shared.used() as i64)))
    };
    let available = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| Value::int(shared.available() as i64)))
    };
    let reset = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| {
            shared.reset();
            Value::undefined()
        }))
    };

    let mut m = HashMap::new();
    m.insert("allocateFloat32Array".to_string(), allocate_f32);
    m.insert("allocateFloat64Array".to_string(), allocate_f64);
    m.insert("allocateInt32Array".to_string(), allocate_i32);
    m.insert("allocBytes".to_string(), alloc_bytes);
    m.insert("readFloat32".to_string(), read_f32);
    m.insert("writeFloat32".to_string(), write_f32);
    m.insert("readFloat64".to_string(), read_f64);
    m.insert("writeFloat64".to_string(), write_f64);
    m.insert("readInt32".to_string(), read_i32);
    m.insert("writeInt32".to_string(), write_i32);
    m.insert("readUint8".to_string(), read_u8);
    m.insert("writeUint8".to_string(), write_u8);
    m.insert("size".to_string(), size);
    m.insert("used".to_string(), used);
    m.insert("available".to_string(), available);
    m.insert("reset".to_string(), reset);
    Value::object(m)
}

/// Seed the value of a global by name (natives for the builtins, undefined
/// for user globals; REPL lines carry values over by name).
/// Wrap a finite integral `f64` result as an int (keeping the SMI kind lanes
/// warm), preserving `-0` (JS `Math.floor(-0)` and `Math.round(-0.4)` are
/// `-0`) and passing NaN/±Infinity through as numbers.
fn num_result(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && !(x == 0.0 && x.is_sign_negative()) {
        Value::int(x as i64)
    } else {
        Value::number(x)
    }
}

/// ES parseInt: trim whitespace, optional sign, ToInt32(radix) (NaN/0/
/// undefined → 0, ±Infinity → 0), hex-prefix auto-detection when the radix
/// resolves to 0 or 16, then parse digits in the chosen radix until the first
/// invalid char. No digits → NaN. `"010"` is decimal (10), `"0b101"` → 0
/// (no binary prefix — that's Number()).
fn js_parse_int(v: &Value, radix: &Value) -> Value {
    let owned = to_string_js(v);
    let s = js_trim(&owned);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
        neg = chars[i] == '-';
        i += 1;
    }
    // ToInt32(radix): NaN/±∞ → 0, truncate, fold mod 2^32.
    let r32 = {
        let x = radix.to_number();
        let t = if x.is_nan() || x.is_infinite() {
            0.0
        } else {
            x.trunc()
        };
        let m = t % 4294967296.0;
        let m = if m < 0.0 { m + 4294967296.0 } else { m };
        (m as u32) as i32
    };
    let rest = &chars[i..];
    let hex = rest.len() >= 2
        && rest[0] == '0'
        && (rest[1] == 'x' || rest[1] == 'X');
    let radix = if r32 == 0 {
        if hex {
            i += 2;
            16
        } else {
            10
        }
    } else if r32 == 16 && hex {
        i += 2;
        16
    } else {
        r32
    };
    if !(2..=36).contains(&radix) {
        return Value::number(f64::NAN);
    }
    let mut acc: f64 = 0.0;
    let mut any = false;
    for c in &chars[i..] {
        match c.to_digit(radix as u32) {
            Some(d) => {
                acc = acc * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return Value::number(f64::NAN);
    }
    Value::number(if neg { -acc } else { acc })
}

/// ES parseFloat: trim whitespace, optional sign, `Infinity` literal, then the
/// longest decimal prefix (mantissa + optional fraction + optional exponent).
/// At least one mantissa digit is required, else NaN. The prefix is parsed
/// with the engine's JS-number parser (handles ".5", "5.", "1.e3").
fn js_parse_float(v: &Value) -> Value {
    let owned = to_string_js(v);
    let s = js_trim(&owned);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
        neg = chars[i] == '-';
        i += 1;
    }
    if chars.len() - i >= 8 && chars[i..i + 8] == ['I', 'n', 'f', 'i', 'n', 'i', 't', 'y'] {
        return Value::number(if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let start = i;
    let mut digits = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        let mut j = i + 1;
        let mut edigits = 0usize;
        if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
            j += 1;
        }
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
            edigits += 1;
        }
        if edigits > 0 {
            i = j;
        }
    }
    if digits == 0 {
        return Value::number(f64::NAN);
    }
    let prefix: String = chars[start..i].iter().collect();
    let n = alloy_core::value::js_string_to_number(&prefix);
    Value::number(if neg { -n } else { n })
}

/// Math.random as a native: xorshift64* seeded from time + a counter (the
/// VM thread is the only caller). Must return a *closure* — a bare number
/// would be dispatched as a bytecode entry pointer if called.
fn make_random() -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0);
    Value::native(Arc::new(move |_args, _vm| {
        let seed = if SEED.load(Ordering::Relaxed) == 0 {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15);
            SEED.store(t | 1, Ordering::Relaxed);
            t | 1
        } else {
            SEED.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed)
        };
        let mut x = seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let r = x.wrapping_mul(0x2545F4914F6CDD1D);
        Value::number((r >> 11) as f64 / (1u64 << 53) as f64)
    }))
}

/// `Math` global: floor/ceil/round/abs/sqrt/pow/min/max/random with JS
/// semantics — round half-away-from-zero with `-0` for negatives in
/// `[-0.5, 0)`, min/max return NaN on any NaN argument and track `-0`
/// (min(-0, 0) → -0, max(-0, 0) → 0), min()/max() with no args →
/// +Infinity/-Infinity.
/// The `Map` constructor: a native that carries `Map.prototype` as its
/// prototype, so `new Map()` builds an instance whose proto chain reaches it
/// (`m instanceof Map`) and `Map.prototype` reads back the same object.
/// Iterable-seed arguments (Node accepts `new Map([[k, v], …])`) are not
/// supported — the ctor ignores its args, matching the PRD's cache-server
/// usage (`m.set(k, v)` after construction).
fn make_map_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    // The methods live ONCE on the prototype; each instance shares them and
    // the natives read their instance from `this`. (`size` stays a computed
    // per-read property — see `container_prop`.) `Map.prototype.forEach` etc.
    // are therefore real, matching Node's surface.
    for name in ["get", "set", "has", "delete", "clear", "keys", "values", "entries", "forEach"] {
        if let Some(od) = proto.as_object() {
            od.borrow_mut().set(name, container_method_native(name));
        }
    }
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |_args: &[Value], _vm: &mut dyn VmHost| Value::map(ctor_proto.clone(), 1));
    Value::native_ctor(ctor, proto)
}

/// All seven standard error constructors, Error first (the others chain
/// their prototypes to Error.prototype so `e instanceof Error` holds). Built
/// fresh per call — each is a `Value` allocated in the caller's heap.
fn error_ctor_map() -> hashbrown::HashMap<String, Value> {
    let error = make_error_ctor("Error", Value::undefined());
    let ep = error.as_native_proto().unwrap_or(Value::undefined());
    let mut m = HashMap::new();
    m.insert("Error".to_string(), error);
    for name in [
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
    ] {
        m.insert(name.to_string(), make_error_ctor(name, ep.clone()));
    }
    m
}

/// One standard error constructor. Instances are objects marked
/// `container = 3` with own `name`, `message`, and `stack` properties and a
/// prototype (chained to `parent` — Error.prototype for subclasses) carrying
/// `toString`. Both `new TypeError("x")` and `TypeError("x")` build the
/// instance; `e instanceof TypeError` walks the proto chain; and display /
/// string coercion shows `TypeError: x` (Node's shape).
fn make_error_ctor(name: &str, parent: Value) -> Value {
    let name_owned = name.to_string();
    let proto = Value::object_with_proto(parent);
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        od.set("name", Value::string(name_owned.clone()));
        od.set("message", Value::string(String::new()));
    }
    // `Error.prototype.toString()`: "Name: message" (just "Name" when the
    // message is empty).
    let proto_name = name_owned.clone();
    let to_string = Value::native(Arc::new(move |_args, vm| {
        let this = vm.this_value();
        let (n, m) = match this.as_object() {
            Some(od) => {
                let od = od.borrow();
                let n = od
                    .get("name")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| proto_name.clone());
                let m = od
                    .get("message")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                (n, m)
            }
            None => (proto_name.clone(), String::new()),
        };
        Value::string(if m.is_empty() { n } else { format!("{}: {}", n, m) })
    }));
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        od.set("toString", to_string);
    }
    let proto_for_ctor = proto.clone();
    let ctor_name = name_owned.clone();
    let ctor = Arc::new(move |args: &[Value], _vm: &mut dyn VmHost| {
        let msg = match args.first() {
            Some(v) if v.is_undefined() => String::new(),
            Some(v) => to_string_js(v),
            None => String::new(),
        };
        let obj = Value::object_with_proto(proto_for_ctor.clone());
        {
            let od = obj.as_object().unwrap();
            let mut od = od.borrow_mut();
            od.container = 3;
            od.set("name", Value::string(ctor_name.clone()));
            od.set("message", Value::string(msg.clone()));
            od.set(
                "stack",
                Value::string(if msg.is_empty() {
                    ctor_name.clone()
                } else {
                    format!("{}: {}\n    at <anonymous>", ctor_name, msg)
                }),
            );
        }
        obj
    });
    Value::native_ctor(ctor, proto)
}

/// The `Set` constructor — same shape as [`make_map_ctor`] with container 2.
fn make_set_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    for name in ["add", "has", "delete", "clear", "keys", "values", "entries", "forEach"] {
        if let Some(od) = proto.as_object() {
            od.borrow_mut().set(name, container_method_native(name));
        }
    }
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |_args: &[Value], _vm: &mut dyn VmHost| Value::map(ctor_proto.clone(), 2));
    Value::native_ctor(ctor, proto)
}

fn make_math_module() -> Value {
    let unary = |f: fn(f64) -> f64| -> Value {
        Value::native(Arc::new(move |args, _vm| {
            let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
            num_result(f(x))
        }))
    };
    let floor = unary(f64::floor);
    let ceil = unary(f64::ceil);
    let abs = unary(f64::abs);
    let sqrt = unary(f64::sqrt);
    let round = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let r = (x + 0.5).floor();
        if r == 0.0 && x < 0.0 {
            Value::number(-0.0)
        } else {
            num_result(r)
        }
    }));
    let pow = Value::native(Arc::new(|args, _vm| {
        let a = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let b = args.get(1).cloned().unwrap_or(Value::undefined()).to_number();
        num_result(a.powf(b))
    }));
    let min = Value::native(Arc::new(|args, _vm| {
        let mut best = f64::INFINITY;
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                return Value::number(f64::NAN);
            }
            if n < best || (n == best && n.is_sign_negative() && !best.is_sign_negative()) {
                best = n;
            }
        }
        num_result(best)
    }));
    let max = Value::native(Arc::new(|args, _vm| {
        let mut best = f64::NEG_INFINITY;
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                return Value::number(f64::NAN);
            }
            if n > best || (n == best && best.is_sign_negative() && !n.is_sign_negative()) {
                best = n;
            }
        }
        num_result(best)
    }));
    // Extra-unary wrappers that need NaN + zero handling beyond a bare fn.
    let trunc = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        num_result(x.trunc())
    }));
    let sign = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        if x.is_nan() {
            Value::number(f64::NAN)
        } else if x == 0.0 {
            Value::number(x) // preserves -0
        } else if x > 0.0 {
            Value::int(1)
        } else {
            Value::int(-1)
        }
    }));
    let cbrt = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        num_result(x.cbrt())
    }));
    let hypot = Value::native(Arc::new(|args, _vm| {
        let mut acc = 0.0f64;
        for a in args {
            let n = a.to_number();
            acc = acc.hypot(n);
        }
        num_result(acc)
    }));
    let imul = Value::native(Arc::new(|args, _vm| {
        let a = args.first().cloned().unwrap_or(Value::undefined()).to_number() as u32;
        let b = args.get(1).cloned().unwrap_or(Value::undefined()).to_number() as u32;
        Value::int((a.wrapping_mul(b)) as i32 as i64)
    }));
    let clz32 = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number() as u32;
        Value::int(x.leading_zeros() as i64)
    }));
    let fround = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        Value::number(x as f32 as f64)
    }));
    let atan2 = Value::native(Arc::new(|args, _vm| {
        let y = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let x = args.get(1).cloned().unwrap_or(Value::undefined()).to_number();
        num_result(y.atan2(x))
    }));
    let mut m = HashMap::new();
    // Constants.
    m.insert("E".to_string(), Value::number(std::f64::consts::E));
    m.insert("LN10".to_string(), Value::number(std::f64::consts::LN_10));
    m.insert("LN2".to_string(), Value::number(std::f64::consts::LN_2));
    m.insert("LOG10E".to_string(), Value::number(std::f64::consts::LOG10_E));
    m.insert("LOG2E".to_string(), Value::number(std::f64::consts::LOG2_E));
    m.insert("PI".to_string(), Value::number(std::f64::consts::PI));
    m.insert("SQRT1_2".to_string(), Value::number(std::f64::consts::FRAC_1_SQRT_2));
    m.insert("SQRT2".to_string(), Value::number(std::f64::consts::SQRT_2));
    // Unary f64 functions.
    for (name, f) in [
        ("floor", floor),
        ("ceil", ceil),
        ("abs", abs),
        ("sqrt", sqrt),
        ("sin", unary(f64::sin)),
        ("cos", unary(f64::cos)),
        ("tan", unary(f64::tan)),
        ("asin", unary(f64::asin)),
        ("acos", unary(f64::acos)),
        ("atan", unary(f64::atan)),
        ("log", unary(f64::ln)),
        ("exp", unary(f64::exp)),
        ("log2", unary(f64::log2)),
        ("log10", unary(f64::log10)),
        ("log1p", unary(f64::ln_1p)),
        ("expm1", unary(f64::exp_m1)),
        ("sinh", unary(f64::sinh)),
        ("cosh", unary(f64::cosh)),
        ("tanh", unary(f64::tanh)),
        ("asinh", unary(f64::asinh)),
        ("acosh", unary(f64::acosh)),
        ("atanh", unary(f64::atanh)),
        ("cbrt", cbrt),
        ("trunc", trunc),
        ("sign", sign),
        ("fround", fround),
    ] {
        m.insert(name.to_string(), f);
    }
    m.insert("round".to_string(), round);
    m.insert("pow".to_string(), pow);
    m.insert("min".to_string(), min);
    m.insert("max".to_string(), max);
    m.insert("hypot".to_string(), hypot);
    m.insert("imul".to_string(), imul);
    m.insert("clz32".to_string(), clz32);
    m.insert("atan2".to_string(), atan2);
    m.insert("random".to_string(), make_random());
    Value::object(m)
}

/// `Number` global: parseInt/parseFloat (identical natives to the top-level
/// globals) and isNaN (type-strict — only the NaN number, no coercion).
fn make_number_module() -> Value {
    let parse_int = Value::native(Arc::new(|args, _vm| {
        js_parse_int(
            &args.first().cloned().unwrap_or(Value::undefined()),
            &args.get(1).cloned().unwrap_or(Value::undefined()),
        )
    }));
    let parse_float = Value::native(Arc::new(|args, _vm| {
        js_parse_float(&args.first().cloned().unwrap_or(Value::undefined()))
    }));
    let is_nan = Value::native(Arc::new(|args, _vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        Value::bool(v.is_number() && v.to_number().is_nan())
    }));
    let mut m = HashMap::new();
    m.insert("parseInt".to_string(), parse_int);
    m.insert("parseFloat".to_string(), parse_float);
    m.insert("isNaN".to_string(), is_nan);
    Value::object(m)
}

/// Is `s` a canonical ECMAScript array index ("0".."4294967294", no leading
/// zeros)? Such keys enumerate FIRST — ascending — in Object.keys/values/
/// entries and JSON.stringify; all other string keys follow in insertion
/// order.
fn is_array_index_key(s: &str) -> bool {
    if s.is_empty() || s.len() > 10 {
        return false;
    }
    if s == "0" {
        return true;
    }
    if s.starts_with('0') {
        return false;
    }
    s.parse::<u64>().map_or(false, |n| n < 4294967295)
}

/// A plain object's own (non-deleted) property entries in JS enumeration
/// order: integer-index keys ascending, then the remaining string keys in
/// insertion order. Returns (name, offset) pairs.
fn object_keys_js_order(od: &ObjectData) -> Vec<(String, usize)> {
    let mut ints: Vec<(String, usize)> = Vec::new();
    let mut rest: Vec<(String, usize)> = Vec::new();
    for (k, off) in od.shape.keys_by_offset() {
        let off = off as usize;
        if od.deleted[off] {
            continue;
        }
        if is_array_index_key(k) {
            ints.push((k.clone(), off));
        } else {
            rest.push((k.clone(), off));
        }
    }
    ints.sort_by(|a, b| {
        a.0.parse::<u64>()
            .unwrap_or(0)
            .cmp(&b.0.parse::<u64>().unwrap_or(0))
    });
    ints.extend(rest);
    ints
}

/// ToObject + own-enumerable enumeration for Object.keys/values/entries:
/// null/undefined throw a TypeError (like Node), numbers/booleans have no
/// own keys, strings enumerate their character indices (with the character
/// as the value), objects enumerate their shape entries. `None` means a
/// TypeError was thrown.
fn object_own_entries(arg: &Value, vm: &mut dyn VmHost) -> Option<Vec<(String, Value)>> {
    if arg.is_null() || arg.is_undefined() {
        // Node's exact message for both null and undefined.
        vm.throw_exception(Value::string(
            "TypeError: Cannot convert undefined or null to object".to_string(),
        ));
        return None;
    }
    if let Some(od) = arg.as_object() {
        let od = od.borrow();
        return Some(
            object_keys_js_order(&od)
                .into_iter()
                .map(|(k, off)| (k, unwrap_cell(od.values[off].clone())))
                .collect(),
        );
    }
    if let Some(s) = arg.as_str() {
        return Some(
            s.chars()
                .enumerate()
                .map(|(i, c)| (i.to_string(), Value::string(c.to_string())))
                .collect(),
        );
    }
    Some(Vec::new())
}

/// `Object` global: keys/values/entries with Node's enumeration order and
/// coercion rules (the Map/Set iteration surface already returns array
/// snapshots; this extends the same shape to plain objects).
fn make_object_module() -> Value {
    let keys = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(entries) => Value::array(
                entries.into_iter().map(|(k, _)| Value::string(k)).collect(),
            ),
            None => Value::undefined(),
        }
    }));
    let values = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(entries) => {
                Value::array(entries.into_iter().map(|(_, v)| v).collect())
            }
            None => Value::undefined(),
        }
    }));
    let entries = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(list) => Value::array(
                list.into_iter()
                    .map(|(k, v)| Value::array(vec![Value::string(k), v]))
                    .collect(),
            ),
            None => Value::undefined(),
        }
    }));
    let mut m = HashMap::new();
    m.insert("keys".to_string(), keys);
    m.insert("values".to_string(), values);
    m.insert("entries".to_string(), entries);
    Value::object(m)
}

/// JSON string escaping (RFC 8259): quotes, backslash, the classic short
/// escapes, and control chars as `\u00XX`. Non-ASCII passes through raw,
/// like Node. (The HTTP serializer has its own `json_escape` without the
/// surrounding quotes — this one, for JSON.stringify, includes them.)
fn json_stringify_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `replacer` argument of JSON.stringify: no filter, a key whitelist
/// (array of strings/numbers, applied to every object at every nesting level
/// — arrays always pass all elements), or a function called as `(key, value)`
/// for the root and every property/element before serialization.
#[derive(Clone)]
enum JsonReplacer {
    None,
    Keys(Vec<String>),
    Func(Value),
}

/// Serialize `v` per JSON.stringify. Returns None for values that collapse to
/// undefined (undefined/function at the top level — inside arrays they become
/// null and inside objects the entry is omitted). `visited` is the cycle
/// stack: re-entering a box currently being serialized throws (JS TypeError).
/// `key` is the property name for the function replacer's first argument (""
/// at the root).
fn json_serialize(
    v: &Value,
    key: &str,
    depth: usize,
    indent: &str,
    replacer: &JsonReplacer,
    visited: &mut Vec<u64>,
    vm: &mut dyn VmHost,
) -> Option<String> {
    // Function replacer: transform (or drop) every value before serializing,
    // including the root (key ""). Undefined/function results collapse the
    // same way as the source value would.
    let v = match replacer {
        JsonReplacer::Func(f) => {
            let r = vm.call_value(f, &[Value::string(key.to_string()), v.clone()]);
            if r.is_undefined() {
                return None;
            }
            r
        }
        _ => v.clone(),
    };
    if v.is_undefined() || v.as_function().is_some() || v.is_native() {
        return None;
    }
    if v.is_null() {
        return Some("null".to_string());
    }
    if let Some(b) = v.as_bool() {
        return Some(if b { "true" } else { "false" }.to_string());
    }
    if v.is_number() {
        return Some(alloy_core::value::number_to_string(v.to_number()));
    }
    if let Some(i) = v.as_int() {
        return Some(i.to_string());
    }
    if let Some(s) = v.as_str() {
        return Some(json_stringify_escape(s));
    }
    if v.is_array() {
        let id = v.bits();
        if visited.contains(&id) {
            vm.throw_exception(Value::string(
                "TypeError: Converting circular structure to JSON".to_string(),
            ));
            return None;
        }
        visited.push(id);
        let vals = v.as_array().unwrap().borrow().to_values();
        let mut parts: Vec<String> = Vec::with_capacity(vals.len());
        for (i, e) in vals.iter().enumerate() {
            match json_serialize(e, &i.to_string(), depth + 1, indent, replacer, visited, vm) {
                Some(s) => parts.push(s),
                None => parts.push("null".to_string()),
            }
        }
        visited.pop();
        if indent.is_empty() {
            return Some(format!("[{}]", parts.join(",")));
        }
        let pad = indent.repeat(depth);
        let pad_in = indent.repeat(depth + 1);
        return Some(format!(
            "[\n{}{}\n{}]",
            pad_in,
            parts.join(&format!(",\n{}", pad_in)),
            pad
        ));
    }
    if let Some(od) = v.as_object() {
        let id = v.bits();
        if visited.contains(&id) {
            vm.throw_exception(Value::string(
                "TypeError: Converting circular structure to JSON".to_string(),
            ));
            return None;
        }
        visited.push(id);
        let od = od.borrow();
        let entries: Vec<(&String, u32)> = od.shape.keys_by_offset();
        let mut parts: Vec<String> = Vec::new();
        for (k, off) in entries {
            if od.deleted[off as usize] {
                continue;
            }
            // Key whitelist: applies to objects at every depth.
            if let JsonReplacer::Keys(keys) = replacer {
                if !keys.iter().any(|s| s == k) {
                    continue;
                }
            }
            // Live-import cells serialize as their current value (an exports
            // object's properties are the module's own storage).
            let val = unwrap_cell(od.values[off as usize].clone());
            match json_serialize(&val, k, depth + 1, indent, replacer, visited, vm) {
                Some(s) => parts.push(format!(
                    "{}:{}{}",
                    json_stringify_escape(k),
                    if indent.is_empty() { "" } else { " " },
                    s
                )),
                None => {}
            }
        }
        visited.pop();
        if indent.is_empty() {
            return Some(format!("{{{}}}", parts.join(",")));
        }
        let pad = indent.repeat(depth);
        let pad_in = indent.repeat(depth + 1);
        return Some(format!(
            "{{\n{}{}\n{}}}",
            pad_in,
            parts.join(&format!(",\n{}", pad_in)),
            pad
        ));
    }
    None
}

/// The `space` argument: a number indents that many spaces (capped at 10), a
/// string is used verbatim (capped at 10 chars), anything else → no pretty
/// printing.
fn json_space(space: &Value) -> String {
    // An int literal (`2`) is a tagged int, not an f64 — check both.
    if space.as_int().is_some() || space.is_number() {
        let n = space.to_number();
        if n.is_finite() && n > 0.0 {
            let k = (n.floor() as usize).min(10);
            " ".repeat(k)
        } else {
            String::new()
        }
    } else if let Some(s) = space.as_str() {
        s.chars().take(10).collect()
    } else {
        String::new()
    }
}

/// JSON.parse: a small recursive-descent parser over `char`s (RFC 8259
/// grammar — no NaN/Infinity/leading zeros/single quotes/trailing commas/
/// unquoted keys; `\uXXXX` escapes incl. surrogate pairs).
struct JsonParser {
    chars: Vec<char>,
    i: usize,
}

impl JsonParser {
    fn ws(&mut self) {
        while self.i < self.chars.len() && matches!(self.chars[self.i], ' ' | '\t' | '\n' | '\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.next() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected '{}'", c))
        }
    }

    fn lit(&mut self, s: &str) -> Result<(), String> {
        for c in s.chars() {
            if self.next() != Some(c) {
                return Err(format!("expected '{}'", s));
            }
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut n = 0u32;
        for _ in 0..4 {
            match self.next().and_then(|c| c.to_digit(16)) {
                Some(d) => n = n * 16 + d,
                None => return Err("bad \\u escape".to_string()),
            }
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.next() {
                Some('"') => return Ok(out),
                Some('\\') => match self.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{C}'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('u') => {
                        let hi = self.hex4()?;
                        if (0xD800..=0xDBFF).contains(&hi) {
                            // Surrogate pair: \uD800-\uDBFF \uDC00-\uDFFF.
                            let save = self.i;
                            if self.next() == Some('\\') && self.next() == Some('u') {
                                let lo = self.hex4()?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    let cp =
                                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                } else {
                                    self.i = save;
                                    out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                                }
                            } else {
                                self.i = save;
                                out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        } else {
                            out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ => return Err("bad escape".to_string()),
                },
                Some(c) if (c as u32) < 0x20 => return Err("control char in string".to_string()),
                Some(c) => out.push(c),
                None => return Err("unterminated string".to_string()),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
        }
        match self.peek() {
            Some('0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err("bad number".to_string()),
        }
        if self.peek() == Some('.') {
            self.i += 1;
            if !self.peek().map_or(false, |c| c.is_ascii_digit()) {
                return Err("bad number fraction".to_string());
            }
            while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.i += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.i += 1;
            }
            if !self.peek().map_or(false, |c| c.is_ascii_digit()) {
                return Err("bad number exponent".to_string());
            }
            while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let s: String = self.chars[start..self.i].iter().collect();
        let n: f64 = s.parse().unwrap_or(f64::NAN);
        // Integral and safe → int; -0 and fractions stay numbers.
        if n.fract() == 0.0 && n.abs() <= 9007199254740992.0 && !(n == 0.0 && s.starts_with('-')) {
            Ok(Value::int(n as i64))
        } else {
            Ok(Value::number(n))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            Some('{') => self.obj(),
            Some('[') => self.arr(),
            Some('"') => Ok(Value::string(self.string()?)),
            Some('t') => {
                self.lit("true")?;
                Ok(Value::bool(true))
            }
            Some('f') => {
                self.lit("false")?;
                Ok(Value::bool(false))
            }
            Some('n') => {
                self.lit("null")?;
                Ok(Value::null())
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => Err("unexpected character".to_string()),
        }
    }

    fn arr(&mut self) -> Result<Value, String> {
        self.expect('[')?;
        self.ws();
        let mut out: Vec<Value> = Vec::new();
        if self.peek() == Some(']') {
            self.i += 1;
            return Ok(Value::array(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            match self.next() {
                Some(',') => self.ws(),
                Some(']') => break,
                _ => return Err("expected ',' or ']'".to_string()),
            }
        }
        Ok(Value::array(out))
    }

    fn obj(&mut self) -> Result<Value, String> {
        self.expect('{')?;
        self.ws();
        let mut keys: Vec<String> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();
        if self.peek() == Some('}') {
            self.i += 1;
            return Ok(Value::object(hashbrown::HashMap::new()));
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err("expected string key".to_string());
            }
            let k = self.string()?;
            self.ws();
            self.expect(':')?;
            vals.push(self.value()?);
            keys.push(k);
            self.ws();
            match self.next() {
                Some(',') => {}
                Some('}') => break,
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
        // Keys arrive in document order — preserve it for round-trip
        // stringify (JS objects keep insertion order).
        Ok(Value::object_ordered(keys.into_iter().zip(vals).collect()))
    }

    fn parse(&mut self) -> Result<Value, String> {
        let v = self.value()?;
        self.ws();
        if self.i != self.chars.len() {
            return Err("trailing characters".to_string());
        }
        Ok(v)
    }
}

/// `JSON` global: stringify (with the `space` pretty-print arg, cycle
/// detection that throws, functions/undefined collapsing) and parse (reviver
/// ignored; syntax errors throw). The throw path runs through the new
/// `VmHost::throw_exception` hook so try/catch catches it like a `throw`.
fn make_json_module() -> Value {
    let stringify = Value::native(Arc::new(move |args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        let space = args.get(2).cloned().unwrap_or(Value::undefined());
        let indent = json_space(&space);
        let replacer = match args.get(1) {
            Some(r) if r.as_array().is_some() => {
                let ad = r.as_array().unwrap().borrow();
                let mut keys: Vec<String> = Vec::new();
                for e in ad.to_values() {
                    // Only strings and numbers count (numbers stringified).
                    if e.as_str().is_some() || e.as_int().is_some() || e.is_number() {
                        keys.push(to_string_js(&e));
                    }
                }
                JsonReplacer::Keys(keys)
            }
            Some(r) if !r.is_undefined() && !r.is_null() => JsonReplacer::Func(r.clone()),
            _ => JsonReplacer::None,
        };
        let mut visited: Vec<u64> = Vec::new();
        match json_serialize(&v, "", 0, &indent, &replacer, &mut visited, vm) {
            Some(s) => Value::string(s),
            None => Value::undefined(),
        }
    }));
    let parse = Value::native(Arc::new(move |args, vm| {
        let text = args.first().cloned().unwrap_or(Value::undefined());
        let s = match text.as_str() {
            Some(s) => s.to_string(),
            None => to_string_js(&text),
        };
        let mut p = JsonParser {
            chars: s.chars().collect(),
            i: 0,
        };
        match p.parse() {
            Ok(v) => v,
            Err(msg) => {
                vm.throw_exception(Value::string(format!("SyntaxError: {}", msg)));
                Value::undefined()
            }
        }
    }));
    let mut m = HashMap::new();
    m.insert("stringify".to_string(), stringify);
    m.insert("parse".to_string(), parse);
    Value::object(m)
}

fn seed_global(name: &str, output: Option<Arc<Mutex<Vec<String>>>>, shared: Arc<SidecarMemory>) -> Value {
    match name {
        "print" => make_print_fn(output),
        "http" => make_http_module(),
        "memory" => make_memory_module(shared),
        "fs" => make_fs_module(),
        "Promise" => make_promise_module(),
        "setTimeout" => make_set_timeout(),
        "setInterval" => make_set_interval(),
        "clearTimeout" => make_clear_timer(),
        "clearInterval" => make_clear_timer(),
        "queueMicrotask" => make_queue_microtask(),
        "console" => make_console(output),
        "Array" => make_array_module(),
        "String" => make_string_module(),
        "channel" => make_channel_module(),
        "spawn" => make_spawn_fn(),
        "require" => Value::native(Arc::new(|args, vm| {
            let path = match args.first() {
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                Some(v) => {
                    vm.throw_exception(Value::string(format!(
                        "TypeError: require() expects a string path, got {}",
                        v.type_name()
                    )));
                    return Value::undefined();
                }
                None => {
                    vm.throw_exception(Value::string("TypeError: require() expects a path".to_string()));
                    return Value::undefined();
                }
            };
            vm.require_module(&path)
        })),
        "reload" => Value::native(Arc::new(|args, vm| {
            let path = match args.first() {
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                _ => return Value::bool(false),
            };
            Value::bool(vm.reload_module(&path))
        })),
        "Date" => make_date_ctor(),
        "Math" => make_math_module(),
        "Map" => make_map_ctor(),
        "Set" => make_set_ctor(),
        "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError"
        | "EvalError" | "URIError" => {
            error_ctor_map().remove(name).unwrap_or(Value::undefined())
        }
        "JSON" => make_json_module(),
        "Number" => make_number_module(),
        "Object" => make_object_module(),
        "parseInt" => Value::native(Arc::new(|args, _vm| {
            js_parse_int(
                &args.first().cloned().unwrap_or(Value::undefined()),
                &args.get(1).cloned().unwrap_or(Value::undefined()),
            )
        })),
        "parseFloat" => Value::native(Arc::new(|args, _vm| {
            js_parse_float(&args.first().cloned().unwrap_or(Value::undefined()))
        })),
        "isNaN" => Value::native(Arc::new(|args, _vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(v.to_number().is_nan())
        })),
        // `sweepSegments()` — reclaim orphaned `alloy_shm_*.tmp` segment
        // files left by crashed runs, without waiting for the automatic
        // sweep's 60s rate-limit window. Safe to call freely (a live
        // process's segment is never touched; young files survive the grace
        // period). Returns `{ files, bytes }` for what was reclaimed — a
        // long-running server can call this between requests and log or
        // alert on `files > 0` to spot leaks from long-ago crashes.
        "sweepSegments" => Value::native(Arc::new(|_args, _vm| {
            let s = alloy_core::shared_memory::sweep_segments_now();
            let mut m = HashMap::new();
            m.insert("files".to_string(), Value::number(s.files as f64));
            m.insert("bytes".to_string(), Value::number(s.bytes as f64));
            Value::object(m)
        })),
        // `finalizePythonEmbed()` — optional clean teardown of the
        // in-process CPython interpreter (ALLOY_PYTHON_EMBED=1 only). Tears
        // down this VM's python pools and calls Py_FinalizeEx; terminal, so
        // a server can shut python down cleanly at exit. After it succeeds,
        // embed mode is off for the process and later `.py` imports fall
        // back to child sidecars. Returns `{ finalized, error,
        // liveBackends }`.
        "finalizePythonEmbed" => Value::native(Arc::new(|_args, vm| vm.finalize_python_embed())),
        "NaN" => Value::number(f64::NAN),
        "Infinity" => Value::number(f64::INFINITY),
        _ => Value::undefined(),
    }
}

/// True for the seven standard error constructor names. Used both by the
/// constructor's group-seeding path and by `seed_global_named`.
fn is_error_name(name: &str) -> bool {
    matches!(
        name,
        "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError"
            | "EvalError" | "URIError"
    )
}

fn date_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// A Date instance: an object with `container = DATE_CONTAINER` holding epoch
/// ms under the reserved `DATE_MS_KEY` property, proto = Date.prototype. The
/// value-layer coercion hooks (value.rs) read that key, so `+d`, `d - d`,
/// `String(d)`, and `d == "Wed …"` behave like Node with no VM involvement.
fn date_instance(ms: f64, proto: Value) -> Value {
    let v = Value::object_with_proto(proto);
    if let Some(od) = v.as_object() {
        let mut od = od.borrow_mut();
        od.container = alloy_core::value::DATE_CONTAINER;
        od.set(alloy_core::value::DATE_MS_KEY, Value::number(ms));
    }
    v
}

/// Epoch ms a `new Date(args…)` / `Date(args…)` call resolves to, with JS
/// argument semantics: no args → now; one number/Date → that time; one
/// string → `Date.parse`; one undefined → Invalid; two or more → local
/// components (0-99 year → 1900+).
fn date_ctor_ms(args: &[Value]) -> f64 {
    match args.len() {
        0 => date_now_ms(),
        1 => {
            let a = &args[0];
            if a.is_undefined() {
                f64::NAN
            } else if let Some(ms) = alloy_core::value::date_ms(a) {
                ms
            } else if a.is_string() {
                alloy_core::value::date_parse(a.as_str().unwrap_or(""))
            } else {
                // null → 0, true → 1, objects/arrays → NaN (ToNumber).
                a.to_number()
            }
        }
        _ => {
            let get = |i: usize| -> f64 { args.get(i).map(|v| v.to_number()).unwrap_or(0.0) };
            let mut y = get(0);
            let (mo, d, h, mi, s, ms) = (get(1), get(2), get(3), get(4), get(5), get(6));
            if [y, mo, d, h, mi, s, ms]
                .iter()
                .any(|v| !v.is_finite())
            {
                return f64::NAN;
            }
            if (0.0..=99.0).contains(&y) {
                y += 1900.0;
            }
            alloy_core::value::ms_from_local_components(
                y as i64,
                mo as i64,
                d as i64,
                h as i64,
                mi as i64,
                s as i64,
                ms as i64,
            )
        }
    }
}

/// The receiver's stored epoch ms for a Date-prototype native; `NaN` when
/// the receiver isn't a Date instance or is an Invalid Date.
fn this_date_ms(vm: &dyn VmHost) -> f64 {
    alloy_core::value::date_ms(&vm.this_value()).unwrap_or(f64::NAN)
}

/// Component getters: `getFullYear`…`getMilliseconds` (local or UTC) plus
/// `getDay` (weekday) and `getTimezoneOffset`. All return NaN on Invalid or
/// a non-Date receiver (the engine's non-throwing style).
fn date_getter_native(comp: &str, utc: bool) -> Value {
    let comp = comp.to_string();
    Value::native(Arc::new(move |_args, vm| {
        let ms = this_date_ms(vm);
        let (y, m, d, h, mi, s, msp) = if utc {
            alloy_core::value::ms_components_utc(ms)
        } else {
            alloy_core::value::ms_components_local(ms)
        };
        let out: f64 = match comp.as_str() {
            "year" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    y as f64
                }
            }
            "month" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    (m - 1) as f64
                }
            }
            "date" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    d as f64
                }
            }
            "day" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    ((ms.floor() as i64).div_euclid(86_400_000).rem_euclid(7) + 4).rem_euclid(7)
                        as f64
                }
            }
            "hours" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    h as f64
                }
            }
            "minutes" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    mi as f64
                }
            }
            "seconds" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    s as f64
                }
            }
            "milliseconds" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    msp as f64
                }
            }
            _ => f64::NAN,
        };
        Value::number(out)
    }))
}

/// Which local/UTC components a setter replaces, in argument order.
const DATE_SET_ORDER: &[&[&str]] = &[
    &["milliseconds"],          // setMilliseconds(ms)
    &["seconds", "milliseconds"], // setSeconds(s, ms)
    &["minutes", "seconds", "milliseconds"], // setMinutes(mi, s, ms)
    &["hours", "minutes", "seconds", "milliseconds"], // setHours(h, mi, s, ms)
    &["date"],                 // setDate(d)
    &["month", "date"],       // setMonth(mo, d)
    &["year", "month", "date"], // setFullYear(y, mo, d)
];

/// Component setters: recompute the stored time from the current components
/// with the named ones replaced (missing/undefined args keep the current
/// value; a NaN component makes the Date invalid). Returns the new ms.
fn date_setter_native(order: &[&str], utc: bool) -> Value {
    let order: Vec<String> = order.iter().map(|s| s.to_string()).collect();
    Value::native(Arc::new(move |args, vm| {
        let this = vm.this_value();
        let cur = this_date_ms(vm);
        let (mut y, mut m, mut d, mut h, mut mi, mut s, mut msp) = if utc {
            alloy_core::value::ms_components_utc(cur)
        } else {
            alloy_core::value::ms_components_local(cur)
        };
        let mut invalid = !cur.is_finite();
        for (i, comp) in order.iter().enumerate() {
            let Some(v) = args.get(i) else { continue };
            if v.is_undefined() {
                continue;
            }
            let n = v.to_number();
            if n.is_nan() || n.is_infinite() {
                invalid = true;
                continue;
            }
            let n = n as i64;
            match comp.as_str() {
                "year" => y = if (0..=99).contains(&n) { n + 1900 } else { n },
                "month" => m = n + 1,
                "date" => d = n,
                "hours" => h = n,
                "minutes" => mi = n,
                "seconds" => s = n,
                "milliseconds" => msp = n,
                _ => {}
            }
        }
        let new_ms = if invalid {
            f64::NAN
        } else if utc {
            alloy_core::value::ms_from_utc_components(y, m - 1, d, h, mi, s, msp)
        } else {
            alloy_core::value::ms_from_local_components(y, m - 1, d, h, mi, s, msp)
        };
        alloy_core::value::date_set_ms(&this, new_ms);
        Value::number(new_ms)
    }))
}

/// One shared Date method native installed on Date.prototype; the receiver
/// comes from `this` like the Map/Set methods.
fn date_method_native(name: &str) -> Value {
    match name {
        "getTime" | "valueOf" => Value::native(Arc::new(|_args, vm| {
            Value::number(this_date_ms(vm))
        })),
        "getTimezoneOffset" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::number(
                -(alloy_core::value::local_offset_ms(ms) as f64) / 60_000.0,
            )
        })),
        "getFullYear" => date_getter_native("year", false),
        "getMonth" => date_getter_native("month", false),
        "getDate" => date_getter_native("date", false),
        "getDay" => date_getter_native("day", false),
        "getHours" => date_getter_native("hours", false),
        "getMinutes" => date_getter_native("minutes", false),
        "getSeconds" => date_getter_native("seconds", false),
        "getMilliseconds" => date_getter_native("milliseconds", false),
        "getUTCFullYear" => date_getter_native("year", true),
        "getUTCMonth" => date_getter_native("month", true),
        "getUTCDate" => date_getter_native("date", true),
        "getUTCDay" => date_getter_native("day", true),
        "getUTCHours" => date_getter_native("hours", true),
        "getUTCMinutes" => date_getter_native("minutes", true),
        "getUTCSeconds" => date_getter_native("seconds", true),
        "getUTCMilliseconds" => date_getter_native("milliseconds", true),
        "setTime" => Value::native(Arc::new(|args, vm| {
            let this = vm.this_value();
            let n = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
            alloy_core::value::date_set_ms(&this, n);
            Value::number(n)
        })),
        "setMilliseconds" => date_setter_native(DATE_SET_ORDER[0], false),
        "setSeconds" => date_setter_native(DATE_SET_ORDER[1], false),
        "setMinutes" => date_setter_native(DATE_SET_ORDER[2], false),
        "setHours" => date_setter_native(DATE_SET_ORDER[3], false),
        "setDate" => date_setter_native(DATE_SET_ORDER[4], false),
        "setMonth" => date_setter_native(DATE_SET_ORDER[5], false),
        "setFullYear" => date_setter_native(DATE_SET_ORDER[6], false),
        "setUTCMilliseconds" => date_setter_native(DATE_SET_ORDER[0], true),
        "setUTCSeconds" => date_setter_native(DATE_SET_ORDER[1], true),
        "setUTCMinutes" => date_setter_native(DATE_SET_ORDER[2], true),
        "setUTCHours" => date_setter_native(DATE_SET_ORDER[3], true),
        "setUTCDate" => date_setter_native(DATE_SET_ORDER[4], true),
        "setUTCMonth" => date_setter_native(DATE_SET_ORDER[5], true),
        "setUTCFullYear" => date_setter_native(DATE_SET_ORDER[6], true),
        "toString" => Value::native(Arc::new(|_args, vm| {
            Value::string(alloy_core::value::date_to_string(this_date_ms(vm)))
        })),
        "toISOString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            if !ms.is_finite() {
                vm.throw_exception(Value::string(
                    "RangeError: Invalid time value".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(alloy_core::value::date_to_iso_string(ms))
        })),
        "toUTCString" | "toGMTString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_utc_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toDateString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_date_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toTimeString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_time_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                format!(
                    "{}, {}",
                    alloy_core::value::date_to_locale_date_string(ms),
                    alloy_core::value::date_to_locale_time_string(ms)
                )
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleDateString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_locale_date_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleTimeString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_locale_time_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toJSON" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            if !ms.is_finite() {
                return Value::null();
            }
            Value::string(alloy_core::value::date_to_iso_string(ms))
        })),
        _ => Value::undefined(),
    }
}

/// Full `Date`: a callable constructor with the prototype carrying all
/// getters/setters/formatters and statics `now`/`parse`/`UTC` on the
/// constructor itself.
fn make_date_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        for name in [
            "getTime", "getFullYear", "getMonth", "getDate", "getDay", "getHours",
            "getMinutes", "getSeconds", "getMilliseconds", "getTimezoneOffset",
            "getUTCFullYear", "getUTCMonth", "getUTCDate", "getUTCDay", "getUTCHours",
            "getUTCMinutes", "getUTCSeconds", "getUTCMilliseconds", "setTime",
            "setMilliseconds", "setSeconds", "setMinutes", "setHours", "setDate",
            "setMonth", "setFullYear", "setUTCMilliseconds", "setUTCSeconds",
            "setUTCMinutes", "setUTCHours", "setUTCDate", "setUTCMonth", "setUTCFullYear",
            "toString", "toISOString", "toUTCString", "toGMTString", "toDateString",
            "toTimeString", "toLocaleString", "toLocaleDateString", "toLocaleTimeString",
            "toJSON", "valueOf",
        ] {
            od.set(name, date_method_native(name));
        }
    }
    let now = Value::native(Arc::new(|_args, _vm| Value::number(date_now_ms())));
    let parse = Value::native(Arc::new(|args, _vm| {
        let s = args.first().map(|v| to_string_js(v)).unwrap_or_default();
        Value::number(alloy_core::value::date_parse(&s))
    }));
    let utc = Value::native(Arc::new(|args, _vm| {
        let get = |i: usize| -> f64 { args.get(i).map(|v| v.to_number()).unwrap_or(0.0) };
        let mut y = get(0);
        let (mo, d, h, mi, s, ms) = (get(1), get(2), get(3), get(4), get(5), get(6));
        if [y, mo, d, h, mi, s, ms]
            .iter()
            .any(|v| !v.is_finite())
        {
            return Value::number(f64::NAN);
        }
        if (0.0..=99.0).contains(&y) {
            y += 1900.0;
        }
        Value::number(alloy_core::value::ms_from_utc_components(
            y as i64,
            mo as i64,
            d as i64,
            h as i64,
            mi as i64,
            s as i64,
            ms as i64,
        ))
    }));
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |args: &[Value], _vm: &mut dyn VmHost| {
        date_instance(date_ctor_ms(args), ctor_proto.clone())
    });
    Value::native_with_props(
        ctor,
        proto,
        vec![
            ("now".to_string(), now),
            ("parse".to_string(), parse),
            ("UTC".to_string(), utc),
        ],
    )
}

/// Message-passing concurrency primitive: `channel.create()` returns a
/// bidirectional FIFO. `send` resolves the oldest pending `recv()` promise (or
/// buffers), `recv` takes the oldest message (or parks on a promise the event
/// loop resolves when a `send` lands). `await ch.recv()` therefore suspends the
/// async function on an empty channel exactly like a message queue in any
/// actor runtime.
/// Named channels, shared across VMs and threads: `channel.create("jobs")`
/// registers the channel here and `channel.get("jobs")` (from a spawn
/// worker's isolated VM, or the reverse) returns the same state — the only
/// way two VMs can share a channel, since Values (arena pointers) cannot be
/// serialized. Named channels therefore carry their messages as bytes (see
/// `ChannelItem`). Entries live for the process, like a global registry.
static NAMED_CHANNELS: std::sync::OnceLock<
    Mutex<HashMap<String, Arc<Mutex<ChannelState>>>>,
> = std::sync::OnceLock::new();

fn named_channels() -> &'static Mutex<HashMap<String, Arc<Mutex<ChannelState>>>> {
    NAMED_CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn make_channel_module() -> Value {
    let create = Value::native(Arc::new(|args, _vm| {
        let name = args.first().and_then(|v| v.as_str()).map(|s| s.to_string());
        let ch = Value::channel(Arc::new(Mutex::new(ChannelState {
            queue: VecDeque::new(),
            waiters: VecDeque::new(),
            // Named channels cross VMs, so their messages must travel as
            // bytes; anonymous channels stay per-VM and pass raw values.
            named: name.is_some(),
        })));
        if let Some(n) = name {
            let arc = ch.as_channel().expect("channel").clone();
            named_channels()
                .lock()
                .unwrap_or_else(|g| g.into_inner())
                .insert(n, arc);
        }
        ch
    }));
    let get = Value::native(Arc::new(|args, vm| {
        match args.first().and_then(|v| v.as_str()) {
            Some(name) => named_channels()
                .lock()
                .unwrap_or_else(|g| g.into_inner())
                .get(name)
                .map(|a| Value::channel(a.clone()))
                .unwrap_or_else(|| {
                    vm.throw_exception(Value::string(format!(
                        "Error: channel '{}' not found — create it with channel.create('{}')",
                        name, name
                    )));
                    Value::undefined()
                }),
            None => Value::undefined(),
        }
    }));
    let mut m = HashMap::new();
    m.insert("create".to_string(), create);
    m.insert("get".to_string(), get);
    Value::object(m)
}

fn make_promise_module() -> Value {
    let resolve = Value::native(Arc::new(|args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        // Stamped with the creating VM's wake handle so a settlement routed
        // from another thread reaches the right event loop.
        let wake = vm.wake_handle();
        Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Fulfilled(v),
            continuations: Vec::new(),
            owner: wake,
        })))
    }));
    let with_resolvers = Value::native(Arc::new(|_args, vm| {
        let p = vm.new_promise();
        let p_arc = p.as_promise().expect("new_promise").clone();
        let p_resolve = p_arc.clone();
        let p_reject = p_arc.clone();
        let resolve = Value::native(Arc::new(move |args, vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            vm.resolve_promise(&Value::promise(p_resolve.clone()), v);
            Value::undefined()
        }));
        let reject = Value::native(Arc::new(move |args, vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            vm.reject_promise(&Value::promise(p_reject.clone()), v);
            Value::undefined()
        }));
        let mut obj = HashMap::new();
        obj.insert("promise".to_string(), p);
        obj.insert("resolve".to_string(), resolve);
        obj.insert("reject".to_string(), reject);
        Value::object(obj)
    }));
    let mut m = HashMap::new();
    m.insert("resolve".to_string(), resolve);
    m.insert("withResolvers".to_string(), with_resolvers);
    Value::object(m)
}

/// `setTimeout(cb, ms)` — one-shot timer, returns a numeric handle id that
/// `clearTimeout` accepts. `setInterval(cb, ms)` is the repeating variant.
fn make_set_timeout() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        let ms = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0);
        Value::int(vm.schedule_timer(cb, ms, None) as i64)
    }))
}

fn make_set_interval() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        let ms = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0);
        Value::int(vm.schedule_timer(cb, ms, Some(ms)) as i64)
    }))
}

/// `clearTimeout(id)` / `clearInterval(id)`: cancel a pending timer by its
/// handle. Accepts any value; non-numeric ids are ignored.
fn make_clear_timer() -> Value {
    Value::native(Arc::new(|args, vm| {
        if let Some(v) = args.first() {
            let n = v.to_number();
            if n.is_finite() && n >= 0.0 {
                vm.clear_timer(n as u64);
            }
        }
        Value::undefined()
    }))
}

/// `queueMicrotask(cb)`: schedule `cb` to run as soon as the current
/// synchronous execution finishes, before any timers. Implemented on the
/// promise continuation machinery: an anonymous promise whose Callback
/// continuation is enqueued immediately.
fn make_queue_microtask() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        vm.queue_microtask(cb);
        Value::undefined()
    }))
}

/// `console` global: log/info/warn/error/debug write to stdout (via the print
/// sink, so tests capture them); the others mirror Node's shapes.
fn make_console(sink: Option<Arc<Mutex<Vec<String>>>>) -> Value {
    let make = |label: Option<&'static str>| {
        let sink = sink.clone();
        Value::native(Arc::new(move |args, _vm| {
            let parts: Vec<String> = args
                .iter()
                .map(|arg| match arg.as_str() {
                    Some(s) => s.to_string(),
                    None => format!("{}", arg),
                })
                .collect();
            let line = parts.join(" ");
            let line = match label {
                Some(l) => format!("{} {}", l, line),
                None => line,
            };
            match &sink {
                Some(s) => {
                    s.lock().unwrap().push(line);
                }
                None => println!("{}", line),
            }
            Value::undefined()
        }))
    };
    let mut m = HashMap::new();
    m.insert("log".to_string(), make(None));
    m.insert("info".to_string(), make(None));
    m.insert("debug".to_string(), make(None));
    m.insert("warn".to_string(), make(Some("warn:")));
    m.insert("error".to_string(), make(Some("error:")));
    m.insert("trace".to_string(), make(Some("trace:")));
    m.insert("dir".to_string(), make(None));
    Value::object(m)
}

/// `Array` global: `Array.isArray`, `Array.from` (array-likes, strings, and
/// Map/Set), `Array.of`.
fn make_array_module() -> Value {
    let is_array = Value::native(Arc::new(|args, _vm| {
        Value::bool(args.first().map(|v| v.is_array()).unwrap_or(false))
    }));
    let from = Value::native(Arc::new(|args, vm| {
        let src = args.first().cloned().unwrap_or(Value::undefined());
        let map_fn = args
            .get(1)
            .cloned()
            .filter(|v| v.is_function() || v.is_native());
        let mut out: Vec<Value> = Vec::new();
        if let Some(ad) = src.as_array() {
            out = ad.borrow().to_values();
        } else if let Some(s) = src.as_str() {
            out = s.chars().map(|c| Value::string(c.to_string())).collect();
        } else if let Some(m) = src.as_object() {
            let container = m.borrow().container;
            if container == 1 {
                for (k, v) in container_pairs(&src) {
                    out.push(Value::array(vec![k, v]));
                }
            } else if container == 2 {
                for (_, v) in container_pairs(&src) {
                    out.push(v);
                }
            } else if container == 0 {
                // Array-like: numeric `length`, indexed reads.
                let m = src.as_object().unwrap();
                let len = {
                    let m = m.borrow();
                    m.get("length")
                        .map(|v| {
                            let n = v.to_number();
                            if n.is_nan() {
                                0
                            } else {
                                n.trunc().max(0.0) as usize
                            }
                        })
                        .unwrap_or(0)
                };
                for i in 0..len {
                    let m = m.borrow();
                    let v = m.get(&i.to_string()).cloned().unwrap_or(Value::undefined());
                    drop(m);
                    out.push(v);
                }
            }
        }
        if let Some(f) = map_fn {
            for i in 0..out.len() {
                let e = out[i].clone();
                out[i] = vm.call_value(&f, &[e, Value::int(i as i64)]);
            }
        }
        Value::array(out)
    }));
    let of = Value::native(Arc::new(|args, _vm| Value::array(args.to_vec())));
    let mut m = HashMap::new();
    m.insert("isArray".to_string(), is_array);
    m.insert("from".to_string(), from);
    m.insert("of".to_string(), of);
    Value::object(m)
}

/// `String` global: `String(x)` coercion and `String.fromCharCode` /
/// `String.fromCodePoint`.
fn make_string_module() -> Value {
    let from_char_code = Value::native(Arc::new(|args, _vm| {
        let mut out = String::new();
        for a in args {
            let n = a.to_number();
            let n = if n.is_nan() || n <= 0.0 { 0.0 } else { n.trunc() };
            let n = ((n as i64) & 0xFFFF) as u32;
            if let Some(c) = char::from_u32(n) {
                out.push(c);
            }
        }
        Value::string(out)
    }));
    let from_code_point = Value::native(Arc::new(|args, _vm| {
        let mut out = String::new();
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                out.push('\u{FFFD}');
            } else if n < 0.0 || n > 0x10FFFF as f64 || (n.trunc() != n) {
                // Invalid code point: engine throws via the host's next
                // native check — coerce to the replacement char instead.
                out.push('\u{FFFD}');
            } else if let Some(c) = char::from_u32(n as u32) {
                out.push(c);
            } else {
                out.push('\u{FFFD}');
            }
        }
        Value::string(out)
    }));
    // Callable `String(x)` coercion plus statics (`String.fromCharCode`,
    // `String.fromCodePoint`) on the same value via native props.
    Value::native_with_props(
        Arc::new(move |args, _vm| {
            match args.first() {
                Some(v) => Value::string(to_string_js(v)),
                None => Value::string(String::new()),
            }
        }),
        Value::undefined(),
        vec![
            ("fromCharCode".to_string(), from_char_code),
            ("fromCodePoint".to_string(), from_code_point),
        ],
    )
}

/// `spawn(fn, ...args)`: run `fn` on a worker thread in an isolated VM,
/// returning a promise of its result. `fn` and its arguments cross as
/// serialized bytes; the worker and caller share no state — the PRD's
/// concurrency model.
fn make_spawn_fn() -> Value {
    Value::native(Arc::new(|args, vm| {
        let f = args.first().cloned().unwrap_or(Value::undefined());
        let rest = &args[1..];
        vm.spawn_fn(&f, rest)
    }))
}

/// Serialize a value onto the spawn wire (bytecode.rs's value format plus a
/// function tag). Values that cannot cross an isolated thread boundary
/// (natives, channels, promises, cells, shared pointers — and functions when
/// `allow_fn` is false, i.e. in results) coerce to `undefined`: message
/// passing carries data, never references.
///
/// With `allow_fn`, a closure serializes as tag 9 = `[entry u32][cell count
/// u32][cell contents...]` against the envelope's program — the worker
/// rebuilds a fresh function value at the same bytecode entry with fresh
/// cells, so closure environments (captured helpers, counters, config
/// objects) survive the crossing. Functions from a different program cannot
/// cross and coerce to undefined.
fn write_spawn_value(out: &mut Vec<u8>, v: &Value, allow_fn: bool, program_id: u32) {
    if v.is_undefined() {
        out.push(0);
    } else if v.is_null() {
        out.push(1);
    } else if let Some(b) = v.as_bool() {
        out.push(2);
        out.push(b as u8);
    } else if let Some(n) = v.as_number() {
        out.push(3);
        out.extend_from_slice(&n.to_bits().to_be_bytes());
    } else if let Some(i) = v.as_int() {
        out.push(4);
        out.extend_from_slice(&i.to_be_bytes());
    } else if let Some(s) = v.as_str() {
        out.push(5);
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    } else if let Some(id) = v.as_symbol() {
        out.push(6);
        out.extend_from_slice(&id.to_be_bytes());
    } else if let Some(f) = v.as_function() {
        if allow_fn && f.program == program_id {
            out.push(9);
            out.extend_from_slice(&(f.ptr as u32).to_be_bytes());
            out.extend_from_slice(&(f.cells.len() as u32).to_be_bytes());
            for c in &f.cells {
                write_spawn_value(out, &c.borrow(), true, program_id);
            }
        } else {
            out.push(0);
        }
    } else if let Some(arr) = v.as_array() {
        out.push(7);
        let arr = arr.borrow();
        out.extend_from_slice(&(arr.len() as u32).to_be_bytes());
        for e in arr.to_values() {
            write_spawn_value(out, &e, allow_fn, program_id);
        }
    } else if let Some(m) = v.as_object() {
        out.push(8);
        let m = m.borrow();
        out.extend_from_slice(&(m.len() as u32).to_be_bytes());
        for (k, val) in m.iter_sorted() {
            out.extend_from_slice(&(k.len() as u32).to_be_bytes());
            out.extend_from_slice(k.as_bytes());
            write_spawn_value(out, val, allow_fn, program_id);
        }
    } else {
        out.push(0);
    }
}

/// Decode a value from the spawn wire on the worker side. Mirrors
/// bytecode.rs's `decode_value` plus tag 9 (functions), which rebuilds a
/// closure at `entry` in the worker's single program (id 0) with freshly
/// allocated cells.
fn decode_spawn_value(bytes: &[u8], pos: &mut usize) -> Value {
    let tag = bytes[*pos];
    *pos += 1;
    let read_u32 = |bytes: &[u8], p: &mut usize| -> u32 {
        let b = &bytes[*p..*p + 4];
        *p += 4;
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    };
    let read_i64 = |bytes: &[u8], p: &mut usize| -> i64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[*p..*p + 8]);
        *p += 8;
        i64::from_be_bytes(raw)
    };
    match tag {
        0 => Value::undefined(),
        1 => Value::null(),
        2 => {
            let b = bytes[*pos] != 0;
            *pos += 1;
            Value::bool(b)
        }
        3 => {
            let bits = read_i64(bytes, pos) as u64;
            Value::number(f64::from_bits(bits))
        }
        4 => Value::int(read_i64(bytes, pos)),
        5 => {
            let len = read_u32(bytes, pos) as usize;
            let s = String::from_utf8_lossy(&bytes[*pos..*pos + len]).to_string();
            *pos += len;
            Value::string(s)
        }
        6 => Value::symbol(read_i64(bytes, pos) as u64),
        7 => {
            let n = read_u32(bytes, pos) as usize;
            let mut arr = Vec::with_capacity(n);
            for _ in 0..n {
                arr.push(decode_spawn_value(bytes, pos));
            }
            Value::array(arr)
        }
        8 => {
            let n = read_u32(bytes, pos) as usize;
            let mut m = hashbrown::HashMap::with_capacity_and_hasher(n, Default::default());
            for _ in 0..n {
                let len = read_u32(bytes, pos) as usize;
                let k = String::from_utf8_lossy(&bytes[*pos..*pos + len]).to_string();
                *pos += len;
                let v = decode_spawn_value(bytes, pos);
                m.insert(k, v);
            }
            Value::object(m)
        }
        9 => {
            let entry = read_u32(bytes, pos) as usize;
            let n = read_u32(bytes, pos) as usize;
            let mut cells = Vec::with_capacity(n);
            for _ in 0..n {
                cells.push(Rc::new(RefCell::new(decode_spawn_value(bytes, pos))));
            }
            Value::function(FunctionData { program: 0, ptr: entry, params: 0, uses_args: 0, cells, props: RefCell::new(None) })
        }
        _ => Value::undefined(),
    }
}

/// The spawn worker: parse the envelope, run the function in an isolated VM,
/// serialize the result, and send it back. Runs entirely on its own thread —
/// the Vm, its arena heap, and every value it creates are thread-local.
/// A panic (a worker-local bug) is caught and reported as a rejection so the
/// caller's promise never dangles.
fn spawn_worker_thread(
    payload: Vec<u8>,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    id: u64,
    registry: Arc<SharedModuleRegistry>,
    py_registry: Arc<SharedPyRegistry>,
    dir: Option<std::path::PathBuf>,
) {
    let tx2 = tx.clone();
    let result = std::panic::catch_unwind(move || {
        spawn_worker_inner(payload, tx, id, registry, py_registry, dir)
    });
    if result.is_err() {
        // The worker panicked: reject the promise with a marker (status 1,
        // no payload → decode yields undefined).
        let _ = tx2.send((id, vec![1]));
    }
}

fn spawn_worker_inner(
    payload: Vec<u8>,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    id: u64,
    registry: Arc<SharedModuleRegistry>,
    py_registry: Arc<SharedPyRegistry>,
    dir: Option<std::path::PathBuf>,
) {
    let read_u32 = |bytes: &[u8], p: &mut usize| -> u32 {
        let b = &bytes[*p..*p + 4];
        *p += 4;
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    };
    let mut pos = 0usize;
    let plen = read_u32(&payload, &mut pos) as usize;
    let program_bytes = &payload[pos..pos + plen];
    pos += plen;
    let entry = read_u32(&payload, &mut pos) as usize;
    let n = read_u32(&payload, &mut pos) as usize;
    let mut upvalues = Vec::with_capacity(n);
    for _ in 0..n {
        upvalues.push(decode_spawn_value(&payload, &mut pos));
    }
    // Envelope tail: the spawn arguments (decoded like upvalues).
    let nargs = read_u32(&payload, &mut pos) as usize;
    let mut args = Vec::with_capacity(nargs);
    for _ in 0..nargs {
        args.push(decode_spawn_value(&payload, &mut pos));
    }
    // Isolated VM: its own program registry, arena heap, and globals (seeded
    // fresh from the program's names — workers share no caller state).
    let program = match Program::from_bytes(program_bytes) {
        Ok(p) => p,
        Err(_) => {
            let _ = tx.send((id, vec![1]));
            return;
        }
    };
    // The worker VM shares the process module registry, the `.py` generation
    // registry, and the requirer's directory, so `require` inside the spawned
    // function resolves like Node, honors cross-thread `reload()`, and
    // re-imports reloaded python sidecars on its next call.
    let mut vm = Vm::new_worker(program, registry, py_registry, dir);
    // All values the worker creates allocate into its own heap for the whole
    // run (the same guard `run()` establishes).
    let heap_ptr: *mut ArenaHeap = &mut vm.heap;
    let _g = HeapGuard::set(heap_ptr);
    let cells: Vec<Rc<RefCell<Value>>> =
        upvalues.into_iter().map(|v| Rc::new(RefCell::new(v))).collect();
    let f = Value::function(FunctionData { program: 0, ptr: entry, params: 0, uses_args: 0, cells, props: RefCell::new(None) });
    let result = vm.call_value(&f, &args);
    let mut out = Vec::new();
    // A synchronous throw with no handler surfaces as the VM's uncaught
    // exception (dispatch returns undefined); surface it as a rejection.
    if let Some(err) = vm.take_error() {
        out.push(1);
        write_spawn_value(&mut out, &err, false, 0);
    } else if let Some(p) = result.as_promise() {
        // The spawned function is async: drive its own event loop (settled
        // continuations, timers) until the promise settles.
        vm.drive_event_loop();
        // A rejection thrown during pumping (a resumed continuation throwing)
        // also lands in uncaught_exception.
        let status = if let Some(err) = vm.take_error() {
            out.push(1);
            write_spawn_value(&mut out, &err, false, 0);
            None
        } else {
            Some(p.lock().unwrap_or_else(|g| g.into_inner()).status.clone())
        };
        if let Some(status) = status {
            match status {
                PromiseStatus::Fulfilled(v) => {
                    out.push(0);
                    // Results cross as data only: functions inside a result
                    // coerce to undefined (code cannot leave the worker).
                    write_spawn_value(&mut out, &v, false, 0);
                }
                PromiseStatus::Rejected(e) => {
                    out.push(1);
                    write_spawn_value(&mut out, &e, false, 0);
                }
                PromiseStatus::Pending => out.push(2),
            }
        }
    } else {
        out.push(0);
        write_spawn_value(&mut out, &result, false, 0);
    }
    let _ = tx.send((id, out));
}

fn make_fs_module() -> Value {
    let read_file = Value::native(Arc::new(|args, _vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return Value::string(format!(
                    "alloy fs error: expected string path, got {}",
                    args.first().map(|o| o.to_string()).unwrap_or_default()
                ))
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Value::string(s),
            Err(e) => Value::string(format!("alloy fs error: {}", e)),
        }
    }));
    let write_file = Value::native(Arc::new(|args, _vm| {
        let (path, data) = match (args.first().and_then(|v| v.as_str()), args.get(1)) {
            (Some(p), Some(d)) => (p.to_string(), d.clone()),
            _ => return Value::bool(false),
        };
        let text = match data.as_str() {
            Some(s) => s.to_string(),
            None => format!("{}", data),
        };
        Value::bool(std::fs::write(&path, text).is_ok())
    }));
    let exists = Value::native(Arc::new(|args, _vm| {
        let ok = matches!(args.first().and_then(|v| v.as_str()), Some(p) if std::path::Path::new(p).exists());
        Value::bool(ok)
    }));
    let mut m = HashMap::new();
    m.insert("readFileSync".to_string(), read_file);
    m.insert("writeFileSync".to_string(), write_file);
    m.insert("existsSync".to_string(), exists);
    Value::object(m)
}

fn parse_http_request(text: &str) -> (String, String, String) {
    let mut method = "GET".to_string();
    let mut path = "/".to_string();
    if let Some(first) = text.lines().next() {
        let mut parts = first.split_whitespace();
        if let Some(m) = parts.next() {
            method = m.to_string();
        }
        if let Some(p) = parts.next() {
            path = p.to_string();
        }
    }
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (method, path, body)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn serialize_value(v: &Value) -> String {
    if v.is_undefined() {
        "undefined".to_string()
    } else if v.is_null() {
        "null".to_string()
    } else if let Some(b) = v.as_bool() {
        b.to_string()
    } else if let Some(n) = v.as_number() {
        if !n.is_finite() {
            "null".to_string()
        } else if n == (n as i64) as f64 {
            (n as i64).to_string()
        } else {
            n.to_string()
        }
    } else if let Some(i) = v.as_int() {
        i.to_string()
    } else if let Some(s) = v.as_str() {
        format!("\"{}\"", json_escape(s))
    } else if let Some(a) = v.as_array() {
        let a = a.borrow();
        let inner: Vec<String> = a.to_values().iter().map(serialize_value).collect();
        format!("[{}]", inner.join(", "))
    } else if let Some(m) = v.as_object() {
        let m = m.borrow();
        let inner: Vec<String> = m
            .iter_sorted()
            .into_iter()
            .map(|(k, val)| format!("\"{}\": {}", k, serialize_value(val)))
            .collect();
        format!("{{{}}}", inner.join(", "))
    } else {
        "\"native\"".to_string()
    }
}

/// One in-flight HTTP connection: reading the request, running its handler,
/// or done. Reads are non-blocking and incremental, so a client that connects
/// and stalls mid-request never blocks the loop — it just sits here until it
/// sends, closes, or times out.
struct PendingRequest {
    stream: std::net::TcpStream,
    /// Accumulated request bytes while still reading.
    buf: Vec<u8>,
    /// True once the handler was invoked (request fully read).
    started: bool,
    /// `res.send` slot; Some once the handler runs.
    body: Option<Arc<Mutex<Option<String>>>>,
    /// The handler's own promise when it suspended (None for sync handlers
    /// and while still reading).
    done: Option<Value>,
    /// When the connection was accepted; stalled reads are dropped after this
    /// + REQUEST_READ_TIMEOUT so they can't leak connections.
    accepted_at: std::time::Instant,
}

/// A request is complete once its header block ("\r\n\r\n") has arrived and,
/// for requests declaring a body, all Content-Length bytes are in.
fn request_complete(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    let Some(header_end) = text.find("\r\n\r\n") else {
        return false;
    };
    let headers = &text[..header_end];
    let body_start = header_end + 4;
    let content_len = headers
        .lines()
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    text.len().saturating_sub(body_start) >= content_len
}

/// Parse a complete request, invoke the handler, and return the `res.send`
/// slot plus the handler's promise (None for sync handlers).
fn start_handler(
    vm: &mut dyn VmHost,
    handler: &Value,
    req_text: &str,
) -> (Arc<Mutex<Option<String>>>, Option<Value>) {
    let (method, path, body) = parse_http_request(req_text);
    let res_body: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let send_body = res_body.clone();
    let send = Value::native(Arc::new(move |args, _vm| {
        let mut slot = send_body.lock().unwrap();
        *slot = args.first().map(serialize_value);
        Value::undefined()
    }));
    let mut res = HashMap::new();
    res.insert("send".to_string(), send);
    let mut req = HashMap::new();
    req.insert("method".to_string(), Value::string(method));
    req.insert("url".to_string(), Value::string(path));
    req.insert("body".to_string(), Value::string(body));
    req.insert("headers".to_string(), Value::object(HashMap::new()));
    let result = vm.call_value(handler, &[Value::object(req), Value::object(res)]);
    if let Some(err) = vm.take_uncaught_exception() {
        // A synchronous throw inside the handler: surface it as a rejected
        // handler promise so the serve loop responds 500, and clear the
        // uncaught flag — the response *is* the handling, so the VM must not
        // treat it as an uncaught top-level throw that aborts the program.
        let wake = vm.wake_handle();
        let done = Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Rejected(err),
            continuations: Vec::new(),
            owner: wake,
        })));
        return (res_body, Some(done));
    }
    let done = result.as_promise().map(|_| result.clone());
    (res_body, done)
}

fn write_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    let resp = format!(
        "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// Bind the HTTP listener (non-blocking accepts) and return it plus the
/// actual port, so hosts and tests can discover the port before serving.
fn bind_server(port: u16) -> Result<(std::net::TcpListener, u16), String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("alloy http bind error: {}", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("alloy http nonblocking error: {}", e))?;
    let actual = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    println!("alloy http listening on 127.0.0.1:{}", actual);
    Ok((listener, actual))
}

fn serve_http(vm: &mut dyn VmHost, handler: &Value, port: u16) {
    match bind_server(port) {
        Ok((listener, _)) => {
            // The production server lives for the process lifetime, so this
            // flag is never set — it exists so a host (or a test) can stop
            // the loop and let the VM drop cleanly: Vm::drop reaps the
            // python sidecar children (OS processes that would otherwise
            // orphan) and removes the shared-segment file immediately.
            let stop = std::sync::atomic::AtomicBool::new(false);
            serve_loop(vm, handler, listener, &stop);
        }
        Err(e) => eprintln!("{}", e),
    }
}

/// Concurrent request loop: accept everything queued, read requests
/// incrementally (non-blocking), start each handler, pump python completions
/// + microtasks, and write responses for handlers whose promise settled. A
/// slow handler's python call runs on its own worker (same-file calls spread
/// across the file's pool children) while later requests are accepted and
/// started, so no request stalls another; a client that connects and stalls
/// mid-request is parked, never blocks the loop, and times out if it never
/// finishes.
fn serve_loop(
    vm: &mut dyn VmHost,
    handler: &Value,
    listener: std::net::TcpListener,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut pending: Vec<PendingRequest> = Vec::new();
    loop {
        // A host asked us to stop: exit so the owning VM drops. That runs
        // Vm::drop, which reaps the VM's python sidecar children — real OS
        // processes that the arena heap cannot reclaim and that a detached
        // serve thread would orphan — and removes its shared-segment file
        // immediately (a leaked file would otherwise wait for the next
        // startup sweep).
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // 1. Accept everything currently queued. The listener is non-blocking,
        //    so a handler awaiting python never stops new connections; the
        //    accepted streams stay non-blocking for the incremental reads.
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    pending.push(PendingRequest {
                        stream,
                        buf: Vec::with_capacity(512),
                        started: false,
                        body: None,
                        done: None,
                        accepted_at: std::time::Instant::now(),
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // 2. Read available bytes on every unstarted connection; start the
        //    handler once the request is complete. EOF, errors, and stalled
        //    connections (30s) are dropped without blocking anything.
        let mut i = 0;
        while i < pending.len() {
            if pending[i].started {
                i += 1;
                continue;
            }
            if pending[i].accepted_at.elapsed() > std::time::Duration::from_secs(30) {
                pending.remove(i);
                continue;
            }
            let mut chunk = [0u8; 4096];
            match pending[i].stream.read(&mut chunk) {
                Ok(0) => {
                    // Client closed before finishing its request.
                    pending.remove(i);
                    continue;
                }
                Ok(n) => pending[i].buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    i += 1;
                    continue;
                }
                Err(_) => {
                    pending.remove(i);
                    continue;
                }
            }
            if request_complete(&pending[i].buf) {
                let mut pr = pending.remove(i);
                let req_text = String::from_utf8_lossy(&pr.buf).to_string();
                pr.buf.clear();
                let (body, done) = start_handler(vm, handler, &req_text);
                pr.started = true;
                pr.body = Some(body);
                pr.done = done;
                pending.insert(i, pr);
            } else {
                i += 1;
            }
        }
        // 3. One non-blocking pump: settle python completions, run microtasks
        //    (resuming handlers that were awaiting python).
        vm.pump_async();
        // 4. Write responses for every settled request: 200 with `res.send`'s
        //    body, or 500 with the rejection reason when the handler failed.
        let mut i = 0;
        let mut completed = false;
        while i < pending.len() {
            if !pending[i].started {
                i += 1;
                continue;
            }
            let outcome = match &pending[i].done {
                Some(p) => match p.as_promise() {
                    Some(pr) => {
                        let st = pr.lock().unwrap_or_else(|g| g.into_inner());
                        match &st.status {
                            PromiseStatus::Fulfilled(_) => Some(None),
                            PromiseStatus::Rejected(v) => Some(Some(v.clone())),
                            PromiseStatus::Pending => None,
                        }
                    }
                    None => None,
                },
                None => Some(None),
            };
            if let Some(reason) = outcome {
                let mut pr = pending.remove(i);
                completed = true;
                let body = pr.body.as_ref().and_then(|b| b.lock().unwrap().clone());
                match reason {
                    // The handler (or its awaited python call) failed: 500
                    // with the rejection reason as JSON.
                    Some(err_val) => {
                        let body = format!("{{\"error\": {}}}", serialize_value(&err_val));
                        write_response(&mut pr.stream, "HTTP/1.1 500 Internal Server Error", &body);
                    }
                    None => {
                        let body = body.unwrap_or_else(|| "ok".to_string());
                        write_response(&mut pr.stream, "HTTP/1.1 200 OK", &body);
                    }
                }
                // Dropping the request closes the connection (EOF for the
                // client).
            } else {
                i += 1;
            }
        }
        // 5. Per-request unit boundary when anything completed this iteration:
        //    promote what handlers kept and reclaim their garbage.
        if completed {
            vm.promote_generation();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::Compiler;

    fn run_src(src: &str) -> (Vm, Arc<Mutex<Vec<String>>>) {
        let program = Compiler::compile_source(src).expect("compile");
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        (vm, sink)
    }

    /// True when the suite runs with in-process CPython (`ALLOY_PYTHON_EMBED`
    /// set to 1). Child-mode features — the per-call timeout kill, pool
    /// growth, same-file parallelism — don't exist in embed mode (the GIL
    /// serializes and a hung call can't be killed), so tests that assert on
    /// them skip.
    fn embed_mode() -> bool {
        std::env::var("ALLOY_PYTHON_EMBED").as_deref() == Ok("1")
    }

    /// One HTTP request/response round-trip for the server tests. A read
    /// timeout turns a stalled server into an error instead of a hang.
    fn http_client(port: u16, path: &str) -> std::io::Result<String> {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        stream.write_all(format!("GET {} HTTP/1.1\r\nHost: t\r\n\r\n", path).as_bytes())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    /// The arena-backed microtask queue is the GC-killer pattern in action:
    /// a 5000-deep promise chain churns 5000 settled-continuation records, and
    /// a single drain (one cursor reset) reclaims them all — `used()` returns
    /// to zero with no per-record free, and the values survive the round-trip
    /// through the arena.
    #[test]
    fn microtask_arena_bulk_reset_after_chain() {
        let src = r#"
            function step(v) {
                if (v >= 5000) { print("chain done", v); return; }
                return Promise.resolve(v + 1).then(step);
            }
            Promise.resolve(0).then(step);
        "#;
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "chain done 5000");
        // Every settled continuation record was bulk-reclaimed: nothing left
        // in the arena after the queue drained.
        assert_eq!(vm.microtask_arena_used(), 0);
    }

    /// The generational escape analysis: per-run garbage is reclaimed while
    /// values stored in globals, channels, and closures survive promotion.
    /// Run 1 seeds a global cache + channel and churns garbage; run 2 reads
    /// the persisted state and confirms the young generation was reset.
    #[test]
    fn generational_reclaim_between_runs() {
        let run1 = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            let ch = channel.create();
            function store(k, v) { cache[k] = v; }
            store("a", "persisted-string");
            store("n", 42);
            ch.send("queued-msg");
            // Garbage: 2000 strings + 2000 arrays nobody keeps.
            for (let i = 0; i < 2000; i++) {
                let g = "garbage" + i;
                let arr = [g, g];
            }
            print("run1", cache.a, cache.n);
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(run1);
        vm.run();
        let young_after_r1 = vm.heap_used_young();
        // Run 1's garbage must have been reclaimed; only the persisted values
        // were promoted into the old generation.
        assert_eq!(young_after_r1, 0, "young generation not reclaimed after run 1");

        let run2 = Compiler::compile_source_with_mode(
            r#"
            print(cache.a, cache.n, ch.tryRecv());
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run2);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "run1 persisted-string 42\npersisted-string 42 queued-msg");
        assert_eq!(vm.heap_used_young(), 0, "young generation not reclaimed after run 2");
    }

    /// Server-style reclaim: thousands of per-request handler invocations with
    /// `promote_generation` between them. Each request creates garbage plus one
    /// escaped string stored in a global; the young generation must return to
    /// zero after every request while the escaped values survive promotion.
    #[test]
    fn generational_reclaim_between_requests() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let hits = 0;
            let last = "none";
            function handle(req) {
                hits = hits + 1;
                last = "hit" + hits;
                let tmp = [hits, hits, hits];
                return hits;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after setup");

        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        let req = Value::string("req".to_string());
        let mut old_before = 0usize;
        for _ in 0..10_000 {
            vm.call_value(&handler, &[req.clone()]);
            vm.promote_generation();
            assert_eq!(vm.heap_used_young(), 0, "young grew between requests");
            // Old generation only grows with the escaped per-request strings.
            assert!(vm.heap.used_old() > old_before);
            old_before = vm.heap.used_old();
        }

        let check = Compiler::compile_source_with_mode("print(last, hits);", true, false).unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "hit10000 10000");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
    }

    /// The second-generation sweep: a server that replaces a global with a
    /// fresh ~5KB object every request churns the old generation. Without the
    /// major GC the old gen would hold ~10MB of dead objects; with the
    /// non-copying mark-sweep it stays at the live set (free space reused by
    /// the next promotion), and the surviving globals stay readable across
    /// every sweep.
    #[test]
    fn major_gc_compacts_churned_old_gen() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let keep = "keep-this-alive";
            let cache = null;
            function handle(n) {
                let s = "";
                for (let i = 0; i < 512; i++) { s = s + "abcdefghij"; }
                cache = { big: s, n: n };
                return cache.n;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after setup");
        // Force the second-generation sweep to fire constantly (1KB churn).
        vm.major_threshold = 1 << 10;

        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        let req = Value::string("req".to_string());
        let mut peak_old = 0usize;
        for _ in 0..2_000 {
            vm.call_value(&handler, &[req.clone()]);
            vm.promote_generation();
            assert_eq!(vm.heap_used_young(), 0, "young grew between requests");
            peak_old = peak_old.max(vm.heap.used_old());
        }
        // 2000 x ~5KB churned objects would be ~10MB without compaction; the
        // major GC keeps the old gen at the live set (cache + strings).
        assert!(
            peak_old < 1 << 20,
            "old generation grew to {} bytes without reclaiming",
            peak_old
        );

        // Surviving globals are still readable after every compaction.
        let check = Compiler::compile_source_with_mode("print(keep, cache.n);", true, false).unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "keep-this-alive req");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
    }

    /// The non-copying property, measured: a ~10MB live cache in the old
    /// generation must NEVER move — the mark-sweep records addresses in a
    /// set instead of relocating them, so 500 forced majors over a 10MB live
    /// set cost a mark + sweep, not 10MB of copies per major. The surviving
    /// cache is byte-identical (payload addresses stable) across every sweep,
    /// and the old gen stays at the live set while churn reuses free space.
    #[test]
    fn major_gc_big_live_set_never_copies() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let keep = null;
            let cache = {};
            function seed() {
                let s = "";
                for (let i = 0; i < 1024; i++) { s = s + "abcdefghij"; }
                keep = { deep: [1, 2, 3], big: s };
                // ~10MB live: 200 objects each owning its own 50KB growable
                // builder string (a shared 100-byte constant appended 512
                // times — the bytes are copied into each object's buffer, so
                // nothing is shared in the live set). Few, long appends keep
                // the seed fast in debug builds.
                let block = "abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij";
                for (let i = 0; i < 200; i++) {
                    let own = "";
                    for (let j = 0; j < 512; j++) { own = own + block; }
                    cache["k" + i] = { big: own, i: i };
                }
            }
            function churn(n) {
                let s = "";
                for (let i = 0; i < 64; i++) { s = s + "xyz-"; }
                cache["tmp"] = { big: s, n: n };
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        let keep_idx = vm
            .global_names
            .iter()
            .position(|n| n == "keep")
            .expect("keep global");
        // The live cache is resident in old; capture keep's addresses BEFORE
        // any second-generation sweep.
        let keep_before = vm.globals[keep_idx].bits();
        let deep_before = vm.globals[keep_idx]
            .as_object()
            .unwrap()
            .borrow()
            .get("deep")
            .unwrap()
            .bits();
        let live_bytes = vm.heap.used_old();
        assert!(live_bytes > 8 << 20, "expected ~10MB live set, got {}", live_bytes);

        vm.major_threshold = 1 << 10;
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        let t0 = std::time::Instant::now();
        for i in 0..500 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let dt = t0.elapsed();
        eprintln!(
            "major GC over ~{}MB live set: 500 sweeps in {:?} ({:?}/sweep, free_list={} regions, {}B free)",
            live_bytes >> 20,
            dt,
            dt / 500,
            vm.heap.free_list_len(),
            vm.heap.free_bytes()
        );

        // THE non-copying proof: keep's boxes never moved across 500 sweeps.
        let keep_after = vm.globals[keep_idx].bits();
        assert_eq!(
            keep_before, keep_after,
            "live object was copied by the major GC"
        );
        let deep_after = vm.globals[keep_idx]
            .as_object()
            .unwrap()
            .borrow()
            .get("deep")
            .unwrap()
            .bits();
        assert_eq!(deep_before, deep_after, "interior array was copied");
        // Memory stays at the live set; the churned tmp objects reuse free
        // space instead of growing the old gen.
        assert!(
            vm.heap.used_old() < live_bytes + (1 << 20),
            "old gen grew past the live set: {} > {} + 1MB",
            vm.heap.used_old(),
            live_bytes
        );
    }

    /// Constant interning + program-heap compaction: a program full of
    /// repeated strings (property names, literals) must end up with ONE
    /// constant per unique string, the compiler's transient allocations must
    /// be swept out of the program heap, and the surviving constants must
    /// live in the program heap's old generation — where `kind_of` and
    /// `region_at` answer for them directly.
    #[test]
    fn constant_interning_and_program_heap_compaction() {
        use alloy_core::value::AString;
        let src = r#"
            let obj = { longPropertyName: "repeated-literal" };
            let total = 0;
            for (let i = 0; i < 10; i++) {
                total = total + obj.longPropertyName.length + "repeated-literal".length;
            }
            print(total);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        let count = |s: &str| {
            program
                .constants
                .iter()
                .filter(|c| c.as_str() == Some(s))
                .count()
        };
        // Interning: each unique string appears exactly once in the pool.
        assert_eq!(count("repeated-literal"), 1, "string literals must be interned");
        assert_eq!(count("longPropertyName"), 1, "property names must be interned");
        assert_eq!(count("length"), 1, "builtin property names must be interned");
        // Compaction: constants were promoted to the program heap's old gen,
        // the young gen was reset, and every surviving constant is readable
        // there via the side table.
        assert_eq!(program.heap.used_young(), 0, "program heap young not reset after compaction");
        assert!(program.heap.used_old() > 0, "constants not resident in old");
        let mut boxed = 0usize;
        for c in &program.constants {
            if let Some(s) = c.as_str() {
                let addr = ((c.bits() << 16) as i64 >> 16) as usize;
                assert!(program.heap.addr_in_old(addr), "constant box not in old");
                assert_eq!(program.heap.kind_of(addr), 1, "kind_of on the program heap"); // KIND_STRING
                let b = unsafe { &*(addr as *const AString) };
                assert_eq!(b.len(), s.len());
                assert!(program.heap.addr_in_old(b.bytes_ptr() as usize), "string bytes not in old");
                boxed += 1;
            }
        }
        eprintln!(
            "constants: {} slots, {} strings boxed, program heap old={}B young={}B — duplicate constants eliminated by interning",
            program.constants.len(),
            boxed,
            program.heap.used_old(),
            program.heap.used_young()
        );
        // Semantics unchanged: run it and check the output.
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "320");
        assert_eq!(vm.program_id, 0);
        // The .ax round-trip survives: deserialize compacts too, and the
        // constant values remain readable.
        let cloned = program.deep_clone().expect("deep clone");
        assert_eq!(cloned.constants.len(), program.constants.len());
        assert_eq!(cloned.heap.used_young(), 0);
        for (a, b) in program.constants.iter().zip(cloned.constants.iter()) {
            assert_eq!(a.as_str(), b.as_str());
        }
    }

    /// The side-table metadata: payloads are packed at 8-byte alignment with
    /// zero padding between regions (the table — not inline headers — knows
    /// each region's size), so `used_young` equals the sum of the region
    /// spans exactly, and the old scheme's 8-byte-per-region header overhead
    /// is gone entirely.
    #[test]
    fn side_table_young_density_zero_padding() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            function seed() {
                let out = [];
                for (let i = 0; i < 2000; i++) {
                    let s = "str" + i;
                    let a = [i, i + 1, i + 2];
                    let o = { k: s };
                    out.push(s); out.push(a); out.push(o);
                }
                return out;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        // Measure the young arena before any promotion/sweep.
        let mut regions = 0usize;
        let mut payload = 0usize;
        let mut span = 0usize;
        vm.heap.for_each_young_box(|_, _, size| {
            regions += 1;
            payload += size;
            span += (size + 7) & !7;
        });
        let used = vm.heap.used_young();
        eprintln!(
            "young density: {} regions, {}B payload, {}B span, used={}B — zero inter-region padding: {}, header overhead removed: {}B (would be {}B with old 8B headers), bitmap {}B, chunks {}",
            regions,
            payload,
            span,
            used,
            used == span,
            regions * 8,
            span + regions * 8,
            vm.heap.young_dirty_bytes(),
            vm.heap.young_chunk_count()
        );
        assert_eq!(used, span, "regions must pack with zero padding");
        assert!(regions > 4000, "expected a few thousand regions, got {}", regions);
    }

    /// Segregated size-class bins: a churned same-size object must be
    /// reallocated at the SAME address every request (LIFO reuse of its size
    /// class — temporal locality), the dead space must stay coalesced into
    /// one free region (no fragmentation), and `used_old` must stay flat.
    #[test]
    fn size_class_bins_lifo_reuse_pins_churned_slot() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            function churn(n) {
                let s = "";
                for (let i = 0; i < 64; i++) { s = s + "xyz-"; }
                cache["tmp"] = { big: s, n: n };
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        vm.major_threshold = 0; // a major sweep every boundary
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        let cache_idx = vm.global_names.iter().position(|n| n == "cache").expect("cache");
        let tmp_box_addr = |vm: &Vm| -> usize {
            let od = vm.globals[cache_idx].as_object().unwrap();
            let od = od.borrow();
            let tmp = od.get("tmp").unwrap();
            ((tmp.bits() << 16) as i64 >> 16) as usize
        };
        // Warm up until the churned slot reaches its fixed-point cycle.
        for i in 0..64 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let used_at_warmup = vm.heap.used_old();
        let warmup_addrs: std::collections::HashSet<usize> = (0..16)
            .map(|_| {
                vm.call_value(&churn, &[Value::int(0)]);
                vm.promote_generation();
                tmp_box_addr(&vm)
            })
            .collect();
        // Steady state: every subsequent request must reuse one of the
        // warmup addresses (LIFO pinning — the arena never grows new slots)
        // and the old generation must not grow a byte.
        let mut saw_new = 0usize;
        let t0 = std::time::Instant::now();
        for _ in 0..256 {
            vm.call_value(&churn, &[Value::int(0)]);
            vm.promote_generation();
            if !warmup_addrs.contains(&tmp_box_addr(&vm)) {
                saw_new += 1;
            }
        }
        let dt = t0.elapsed();
        eprintln!(
            "size-class churn: steady-state cycle of {} addresses, 256 requests in {:?} ({}µs/req), used_old {} -> {} ({}B drift), free regions={}, free={}B, {} new addresses escaped the cycle",
            warmup_addrs.len(),
            dt,
            dt.as_micros() / 256,
            used_at_warmup,
            vm.heap.used_old(),
            vm.heap.used_old().saturating_sub(used_at_warmup),
            vm.heap.free_list_len(),
            vm.heap.free_bytes(),
            saw_new
        );
        assert_eq!(saw_new, 0, "churned slot escaped its fixed-point cycle");
        assert_eq!(
            vm.heap.used_old(),
            used_at_warmup,
            "old gen grew while churning a fixed-size slot"
        );
        assert!(vm.heap.free_list_len() <= 2, "dead space fragmented: {} regions", vm.heap.free_list_len());
    }

    /// The incremental mark must survive mid-mark mutations: while a
    /// second-generation sweep is being prepared (its worklist spans many
    /// unit boundaries), writes into an already-marked old box (box barrier),
    /// a closure cell (Rc barrier), and a channel queue (Rc barrier) must be
    /// re-traced before the sweep runs, or the freshly-written young values
    /// would be swept out from under the live structures.
    #[test]
    fn incremental_mark_survives_mid_mark_mutation() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            let ch = channel.create();
            let holder = (function () {
                let inner = "initial";
                return {
                    get: function () { return inner; },
                    set: function (v) { inner = v; },
                };
            })();
            function seed() {
                for (let i = 0; i < 2000; i++) { cache["k" + i] = "v" + i; }
            }
            function mutate() {
                cache["fresh"] = { v: "box-barrier" };
                holder.set("cell-barrier");
                ch.send("chan-barrier");
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        let get_global = |vm: &mut Vm, name: &str| -> Value {
            vm.globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == name)
                .map(|(v, _)| v.clone())
                .expect(name)
        };
        // Everything live is in old; force a major at the next boundary with
        // tiny slices so the mark visibly spans many unit boundaries.
        vm.major_threshold = 0;
        vm.mark_budget = 4;
        let seed = get_global(&mut vm, "seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        // The mark is now in progress: ~2000 boxes queued, 4 traced.
        assert!(
            vm.mark.as_ref().is_some_and(|m| m.worklist.len() > 100),
            "mark should be mid-flight with a large worklist"
        );
        // Mutate mid-mark: box write, cell write, channel send.
        let mutate = get_global(&mut vm, "mutate");
        vm.call_value(&mutate, &[]);
        // Drain the mark slice by slice; the young sweep runs every boundary.
        for _ in 0..10_000 {
            vm.promote_generation();
            if vm.mark.is_none() {
                break;
            }
        }
        assert!(vm.mark.is_none(), "mark never drained");
        // Every mid-mark mutation must have survived the sweep.
        let check = Compiler::compile_source_with_mode(
            "print(cache.fresh.v, holder.get(), ch.tryRecv());",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "box-barrier cell-barrier chan-barrier");
    }

    /// Same pattern through `await`: suspended async invocations resume via
    /// arena records, and the channel's event-loop integration resolves a
    /// parked `recv()` from a timer.
    #[test]
    fn channel_async_recv_via_event_loop() {
        let src = r#"
            let ch = channel.create();
            let got = "";
            (async function () { got = got + (await ch.recv()); })();
            setTimeout(() => { ch.send("ping"); }, 5);
            setTimeout(() => { print(got); }, 60);
        "#;
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "ping");
        assert_eq!(vm.microtask_arena_used(), 0);
    }

    /// Rope strings: `s = s + "ab"` in a loop must be O(1) per concat (cons
    /// nodes, zero byte copies) and the lazily-flattened content must be
    /// byte-exact — even for a 100k-leaf left-leaning rope, which a recursive
    /// flatten would stack-overflow on. The iterative flatten materializes
    /// the full expected string.
    #[test]
    fn rope_concat_linear_and_deep_flatten_exact() {
        use alloy_core::heap::{ArenaHeap, HeapGuard};
        use alloy_core::value::AString;
        let mut heap = ArenaHeap::new(1 << 20);
        let _g = HeapGuard::set(&mut heap);
        // Exactly `s = ""; for (...) s = s + "ab";` — a 100k-deep chain of
        // cons boxes. No bytes are copied while building.
        let mut s = Value::string(String::new());
        let tail = Value::string("ab".to_string());
        let start = std::time::Instant::now();
        for _ in 0..100_000 {
            s = Value::rope(s, tail.clone());
        }
        let build_ms = start.elapsed().as_millis();
        // Still a cons node: nothing was flattened or copied during the loop.
        let addr = ((s.bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(b.is_cons(), "loop-built string must stay a rope while building");
        // First read flattens iteratively and must be byte-exact.
        let got = s.as_str().expect("string").to_string();
        let expected: String = "ab".repeat(100_000);
        assert_eq!(got.len(), expected.len());
        assert_eq!(got, expected);
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "first read must flatten the rope in place");
        assert_eq!(b.len(), expected.len());
        eprintln!(
            "rope: 100k concats in {}ms (build), one {}KB flatten, exact content",
            build_ms,
            expected.len() / 1024
        );
    }

    /// Cached rope lengths + mixed concat: `s = s + i` (number!) must stay
    /// a rope while building — the number is converted to a rope leaf, no
    /// per-iteration copy or flatten — and `len()` must be O(1) from the
    /// cached field without flattening or re-walking the tree.
    #[test]
    fn rope_length_cached_and_mixed_concat_ropes() {
        use alloy_core::heap::{ArenaHeap, HeapGuard};
        use alloy_core::value::AString;
        let mut heap = ArenaHeap::new(1 << 20);
        let _g = HeapGuard::set(&mut heap);
        let mut s = Value::string(String::new());
        for i in 0..10_000 {
            s = s.add(&Value::int(i));
        }
        let addr = ((s.bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "mixed loop must not stay a rope tree");
        // len() reads the cached field: correct total, no flatten, no walk.
        let expected: String = (0..10_000).map(|i| i.to_string()).collect();
        assert_eq!(b.len(), expected.len(), "cached length wrong");
        // Content is byte-exact: "012345678910..."
        assert_eq!(s.as_str().unwrap(), expected);
    }

    /// The string-accumulator fusions: `s = s + "x"` / `s += t` / `s = s + e`
    /// each collapse into a single AppendString* opcode (the builder box
    /// stays in the local slot), and the fused semantics match the general
    /// `Add` path byte-for-byte — including self-append, keep-in-expression
    /// contexts, and aliasing (an earlier snapshot must not see later
    /// appends).
    #[test]
    fn string_accumulator_fusion_emitted_and_correct() {
        let src = r#"
            let t = "T";
            let a = "";
            for (let i = 0; i < 1000; i++) { a = a + "x"; }
            a += "!";
            a += t;
            let c = 0;
            for (let i = 0; i < 500; i++) { c = c + (i % 2); }
            let snap = a;
            a += "Z";
            let e = a;
            print(a.length, snap.length, e.length, a[1000], a[1001], a[1002], c);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The fused opcodes must actually be emitted for the accumulator
        // shapes: string-typed RHS (`a = a + "x"`, `a += "!"`, `a += "Z"`)
        // keep AppendStringConst; `a += t` (local leaf) keeps
        // AppendStringLocal; `c = c + (i % 2)` (subtree RHS) keeps
        // AppendStringPop — the register-ALU shapes only claim int/local-const
        // RHS, and every path applies the exact same `Value::add`
        // (rope/growable concat) for strings.
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::AppendStringConst as u8), "AppendStringConst not emitted");
        assert!(has(Opcode::AppendStringLocal as u8), "AppendStringLocal not emitted");
        assert!(has(Opcode::AppendStringPop as u8), "AppendStringPop not emitted");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        // a = 1000 x's + "!" + "T" + "Z" = 1003 chars; snap has 1002
        // (captured before the "Z" append, so it never sees it).
        assert_eq!(out, "1003 1002 1003 ! T Z 250");
    }

    /// The bytecode peephole: `3 * n` (int-on-left), `(i + j) % 7`
    /// (local-local then int), `(x) * 3` (parens defeat the AST fusion), and
    /// `2 * 3 + 1` (constant fold) each collapse into one dispatch, with a
    /// trailing statement Pop folded into keep=0. The loop also proves jump
    /// targets survive the stream compaction, and the outputs match JS.
    #[test]
    fn peephole_fusions_emitted_and_correct() {
        let src = r#"
            function col(n) {
                let steps = 0;
                while (n !== 1) {
                    if (n % 2 === 0) { n = n / 2; }
                    else { n = 3 * n + 1; }
                    steps += 1;
                }
                return steps;
            }
            let total = 0;
            for (let i = 1; i < 40; i++) { total += col(i); }
            let modsum = 0;
            for (let i = 0; i < 100; i++) {
                for (let j = 0; j < 100; j++) { modsum += (i + j) % 7; }
            }
            let px = 3;
            let paren = (px) * 3 + (px) * 5;
            5 * px;
            let fold = 2 * 3 + 1;
            print(total, modsum, paren, fold);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The fused opcodes must actually be emitted: `3 * n` -> BinIntLocal
        // (standalone `5 * px` still hits the peephole's int-on-left pattern),
        // the int-arithmetic trees (`3 * n + 1`, `(i + j) % 7`, paren chains)
        // -> register-ALU ArithChain, and `2 * 3 + 1` -> a single LoadConst
        // (pure-constant trees skip the chain so the fold still precomputes).
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::BinIntLocal as u8), "BinIntLocal not emitted");
        assert!(has(Opcode::ArithChain as u8), "ArithChain not emitted");
        // `2 * 3 + 1` folds to the single constant 7 (byte-level opcode scans
        // would false-positive on operand bytes, so check the constant pool).
        assert!(
            program
                .constants
                .iter()
                .any(|c| c.bits() == Value::int(7).bits()),
            "constant fold did not produce 7 in the constant pool"
        );
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "701 29992 24 7");
    }

    /// Comparison-chain fusion: `a < b && b < c` (and `||`, and every mix of
    /// local/int operands) collapses to CmpAnd* in both value and condition
    /// contexts. The short-circuit value semantics must hold (the second
    /// comparison is never evaluated when the first fires), the retained
    /// JumpPop in condition context must keep working (if/while/for/ternary),
    /// and the outputs must match JS exactly.
    #[test]
    fn peephole_cmp_chains_emitted_and_correct() {
        let src = r#"
            let a = 1, b = 2, c = 3, d = 4;
            let v1 = a < b && b < c;
            let v2 = a < b || b < c;
            let v3 = c < b && b < c;
            let v4 = c < b || b < c;
            let v5 = a < b && c < b;
            let v6 = a < 5 && b < 5;
            let v7 = a < 5 && b < c;
            let v8 = a < b && b < 5;
            let v9 = 5 < a && b < 5;
            let n = 0;
            if (a < b && b < c) { n += 1; }
            if (a < b || c < b) { n += 10; }
            if (c < b && (n = 999)) { }
            let m = 0;
            while (m < 5 && m < 3) { m += 1; }
            let t = a < b && b < c ? 7 : 8;
            a < b && b < c;
            let s = 0;
            for (let i = 0; i < 10 && i < 4; i++) { s += 1; }
            print(v1, v2, v3, v4, v5, v6, v7, v8, v9, n, m, t, s);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::CmpAndLocalLocal as u8), "CmpAndLocalLocal not emitted");
        assert!(has(Opcode::CmpAndLocalInt as u8), "CmpAndLocalInt not emitted");
        assert!(has(Opcode::CmpAndIntLocal as u8), "CmpAndIntLocal not emitted");
        assert!(has(Opcode::CmpAndIntInt as u8), "CmpAndIntInt not emitted");
        // Every comparison-shaped value chain fused — exactly ONE
        // value-context JumpIfFalse instruction may survive: `5 < a && b < 5`
        // has the int on the left, which compiles to a generic
        // LoadInt+LoadLocal+LT (no CmpLocalInt), so that chain is
        // legitimately unfusable. Walk instruction starts (a byte scan would
        // false-positive on operand bytes like a slot of 0x1b).
        let bc = &program.bytecode;
        let mut jifs = 0;
        let mut o = 0;
        while o < bc.len() {
            if bc[o] == Opcode::JumpIfFalse as u8 {
                jifs += 1;
            }
            o += crate::bytecode::op_len(bc, o);
        }
        assert_eq!(jifs, 1, "value-context JumpIfFalse count");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "true true false true false true true true false 11 3 7 4");
    }

    /// Register-ALU fusion: int-arithmetic trees collapse into ONE ArithChain
    /// dispatch that keeps the running value in an i64 register. Verify the
    /// opcode fires on the bench shapes (`3*n+1`, `(lo+hi)%2`, big-mod seed,
    /// compound assigns, nested right subtrees) and that every result matches
    /// Node byte-for-byte, including the generic fallbacks (a float local
    /// kicks the chain out of the i64 lane mid-flight; `-9 % 3` → -0).
    #[test]
    fn arith_chain_registers_emitted_and_correct() {
        let src = r#"
            let a = 7, b = 3;
            let x = a + b * 2 - 1;
            let y = (a + b) % 4;
            let z = 3 * a + 1;
            let lo = 100, hi = 200;
            let mid = (lo + hi - (lo + hi) % 2) / 2;
            let seed = 12345;
            seed = (seed * 48271) % 2147483648;
            let n = 5;
            n = 3 * n + 1;
            let s = 0;
            s += 10 + 5 * 2;
            let t = 0;
            t += a;
            let neg = 0;
            neg -= 5;
            let rem = 1 / (-9 % 3);
            let f = 2.5;
            let r = f + 1 + 2;
            let steps = 0;
            for (let i = 1; i < 100; i++) { steps += 1; }
            print(x, y, z, mid, seed, n, s, t, neg, rem, r, steps);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The bench shapes all fuse: count every register-ALU instruction
        // (the variable ArithChain for expression-position chains, plus the
        // fixed-shape Arith2/3Store* superinstructions for assignments) by
        // walking instruction starts (a raw byte scan would count operand
        // bytes that happen to equal the opcodes).
        let bc = &program.bytecode;
        let mut fused = 0;
        let mut o = 0;
        while o < bc.len() {
            if matches!(
                bc[o],
                b if b == Opcode::ArithChain as u8
                    || b == Opcode::Arith2StoreLocalConst as u8
                    || b == Opcode::Arith3StoreLocalConstConst as u8
                    || b == Opcode::Arith3StoreConstLocalConst as u8
            ) {
                fused += 1;
            }
            o += crate::bytecode::op_len(bc, o);
        }
        assert!(fused >= 8, "only {fused} register-ALU instructions emitted");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        // Verified against Node: 12 2 22 150 595905495 16 20 7 -5 -Infinity
        // 5.5 99 (the engine's display now matches Node's -Infinity).
        assert_eq!(out, "12 2 22 150 595905495 16 20 7 -5 -Infinity 5.5 99");
    }

    /// Cross-thread `spawn(fn)`: the function runs on a worker thread in an
    /// isolated VM and the result settles as a promise on the VM thread.
    /// Covers sync results, rejection propagation, closure environments
    /// (a captured helper function survives the serialized crossing), and an
    /// async spawned function that awaits a timer on the worker's own event
    /// loop.
    #[test]
    fn spawn_runs_on_worker_thread_and_settles_promise() {
        let src = r#"
            function helper() { let m = 5; return function (x) { return x * m; }; }
            async function main() {
                let out = [];
                out.push("sync:" + await spawn(function () {
                    let s = 0;
                    for (let i = 0; i < 1000; i++) { s += i; }
                    return s;
                }));
                out.push("closure:" + await spawn(function () {
                    let h = helper();
                    return h(6);
                }));
                out.push("obj:" + await spawn(function () {
                    return { a: 1, b: [2, 3] };
                }));
                try {
                    await spawn(function () { throw "boom"; });
                    out.push("no-rejection");
                } catch (e) {
                    out.push("rejected:" + e);
                }
                out.push("async:" + await spawn(function () {
                    let w = Promise.withResolvers();
                    setTimeout(function () { w.resolve(21); }, 5);
                    return w.promise.then(function (v) { return v * 2; });
                }));
                print(out.join(" "));
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "sync:499500 closure:30 obj:[object Object] rejected:boom async:42");
    }

    /// Array.prototype.join/push and Date.now: the natives the differential
    /// `require` from inside an async HTTP handler: the module is loaded in
    /// the handler's synchronous portion (before AND after an `await` — the
    /// post-await require runs inside a resumed continuation), and the module
    /// singleton persists across requests (require-cache hit).
    #[test]
    fn require_works_inside_async_server_handler() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_handler_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("counter.ajs"),
            "export let n = 0;\n\
             export function bump() { n = n + 1; return n; }\n",
        )
        .unwrap();
        // top_level_globals: the server thread looks `handle` up in the VM's
        // globals (same as the python server tests).
        let program = Compiler::compile_source_with_mode(
            r#"
            async function handle(req, res) {
                const m = require('./counter.ajs');
                const a = m.bump();
                await Promise.resolve(1);
                const b = m.bump();
                res.send('' + (a * 100 + b));
            }
            "#,
            true,
            false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let body = |resp: &str| resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        // Request 1: a=1, b=2 -> "102". Request 2 (cache hit): a=3, b=4 ->
        // "304" — the module's counter state lives in the module's globals,
        // not the request, so it must carry across requests.
        let r1 = http_client(port, "/").expect("request 1");
        let r2 = http_client(port, "/").expect("request 2");
        assert_eq!(body(&r1), "\"102\"", "got: {}", r1);
        assert_eq!(body(&r2), "\"304\"", "got: {}", r2);
        // Stop the serve thread and join: the VM drops deterministically
        // (python children reaped, shared-segment file removed) instead of
        // lingering on a detached thread.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `require` inside a spawned worker: the worker inherits the requirer's
    /// directory (so './x.ajs' resolves against the calling file, like Node)
    /// and shares the process-wide compiled-module registry — the module the
    /// worker loaded is reusable from the main VM afterwards (one compile,
    /// two VMs).
    #[test]
    fn require_works_inside_spawn_worker() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_worker_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("math.ajs"),
            "export function double(x) { return x * 2; }\n",
        )
        .unwrap();
        let program = Compiler::compile_source(
            r#"
            async function main() {
                const from_worker = await spawn(function () {
                    const m = require('./math.ajs');
                    return m.double(21);
                });
                print('worker:' + from_worker);
                // Same module, main VM: the worker's compile is shared, so
                // this is a registry hit — no recompile, and the module's
                // functions run on the main thread.
                const m = require('./math.ajs');
                print('main:' + m.double(5));
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "worker:42\nmain:10");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-thread `reload()`: the main VM rewrites a module and reloads it;
    /// the main VM's own next require AND a brand-new worker's first require
    /// both see the new code — the shared registry's generation bump
    /// invalidates every thread's cached copy, and the dropped bytes force a
    /// recompile from the current file.
    #[test]
    fn reload_propagates_across_threads() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_xthread_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("versioned.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        // fs natives resolve relative paths against the process cwd (Node
        // semantics), so rewrite via an absolute path; `require` resolves
        // against the module dir (also Node semantics) and finds the same
        // file through the temp dir.
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const fslib = require('fs');
                const m1 = require('./versioned.ajs');
                print('v1:' + m1.get());
                // Rewrite the module on disk, then reload: invalidates the
                // shared registry for every thread.
                print('wrote:' + fslib.writeFileSync('{}', 'export function get() {{ return 2; }}'));
                print('reloaded:' + reload('./versioned.ajs'));
                // The main VM's own next require re-runs the file -> v2.
                const m2 = require('./versioned.ajs');
                print('main:' + m2.get());
                // A fresh worker whose first require happens AFTER the reload
                // must see v2 too (registry miss -> compile from disk).
                const w = await spawn(function () {{
                    const m = require('./versioned.ajs');
                    return m.get();
                }});
                print('worker:' + w);
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "v1:1\nwrote:true\nreloaded:true\nmain:2\nworker:2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload from *inside* a worker (its own `reload()` native) makes the
    /// worker's very next require re-run the module — the live-thread half of
    /// the multi-tenant story, fully deterministic with no cross-thread wake
    /// needed.
    #[test]
    fn reload_from_worker_invalidates_worker_cache() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_worker_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("worker_mod.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const r = await spawn(function () {{
                    const fslib = require('fs');
                    const a = require('./worker_mod.ajs').get();
                    fslib.writeFileSync('{}', 'export function get() {{ return 2; }}');
                    reload('./worker_mod.ajs');
                    const b = require('./worker_mod.ajs').get();
                    return [a, b];
                }});
                print(r.join(','));
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "1,2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Three workers require the same fresh module at the same time: no
    /// double compile (the registry lock serializes the first), no corruption,
    /// and each worker gets its own module instance (Node's worker model —
    /// per-worker module state), so all three see a clean counter.
    #[test]
    fn concurrent_require_same_module_no_race() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_race_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shared.ajs"),
            "export let n = 0;\n\
             export function bump() { n = n + 1; return n; }\n",
        )
        .unwrap();
        let program = Compiler::compile_source(
            r#"
            async function main() {
                const f = function () {
                    const m = require('./shared.ajs');
                    return m.bump();
                };
                const a = spawn(f);
                const b = spawn(f);
                const c = spawn(f);
                const r1 = await a;
                const r2 = await b;
                const r3 = await c;
                print(r1, r2, r3);
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "1 1 1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE cross-thread wake scenario: a spawn worker parks on
    /// `await ch.recv()` (its event loop would previously break immediately
    /// and the message would be lost); the main thread sends, the worker is
    /// woken via the routed inbox, resumes, and receives BOTH messages. The
    /// worker parks again between the two sends, so both the first wake and
    /// a re-park + second wake are exercised.
    #[test]
    fn worker_parked_on_channel_recv_wakes_on_send() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                const ch = channel.create("wake_test_worker");
                const p = spawn(function () {
                    const c = channel.get("wake_test_worker");
                    return (async function () {
                        const a = await c.recv();
                        const b = await c.recv();
                        return [a, b];
                    })();
                });
                // Give the worker time to park on its first recv; either way
                // (parked → wake, or not yet → buffered bytes) the result is
                // deterministic, but 100ms makes the wake path the likely one.
                await sleep(100);
                ch.send("first");
                ch.send("second");
                print((await p).join(","));
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "first,second");
    }

    /// Reverse direction: the MAIN VM parks on `await ch.recv()` while a
    /// worker sleeps, then sends. The worker's send routes to the main VM's
    /// inbox and wakes its event loop (which is keeping itself alive because
    /// the waiter is parked), so the parked `await` resumes.
    #[test]
    fn main_parked_on_channel_recv_woken_by_worker_send() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                const ch = channel.create("wake_test_main");
                const p = spawn(function () {
                    const c = channel.get("wake_test_main");
                    return (async function () {
                        await sleep(30);
                        c.send("from-worker");
                        return "sent";
                    })();
                });
                const got = await ch.recv();
                const s = await p;
                print(got + " " + s);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "from-worker sent");
    }

    /// Worker-to-worker: two spawned VMs share a named channel; the consumer
    /// parks on `recv`, and the producer (a DIFFERENT VM) sends — routed to
    /// the consumer's inbox and woken there, never touching the main thread.
    #[test]
    fn worker_to_worker_channel_wakes_consumer() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                channel.create("wake_test_w2w");
                const consumer = spawn(function () {
                    const c = channel.get("wake_test_w2w");
                    return (async function () { return await c.recv(); })();
                });
                // Let the consumer park; the producer is a separate VM.
                await sleep(100);
                const producer = spawn(function () {
                    const c = channel.get("wake_test_w2w");
                    c.send("relay");
                    return "done";
                });
                print(await consumer, await producer);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "relay done");
    }

    /// Buffered named-channel messages: a worker sends structured data
    /// (object, array, scalar) BEFORE anyone is waiting. The named channel
    /// serializes each message to bytes on send; the main VM decodes them
    /// into its own heap on recv — the cross-heap path with no wake needed.
    #[test]
    fn named_channel_buffers_structured_messages_as_bytes() {
        let src = r#"
            async function main() {
                const ch = channel.create("buff_test");
                const p = spawn(function () {
                    const c = channel.get("buff_test");
                    c.send({ n: 42, s: "hi" });
                    c.send([1, 2, 3]);
                    c.send(7);
                    return "produced";
                });
                await p;
                const a = await ch.recv();
                const b = await ch.recv();
                const c = await ch.recv();
                print(a.n, a.s, b.join("-"), c);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "42 hi 1-2-3 7");
    }

    /// The two cross-thread features compose: a LIVE worker observes a
    /// reload from the main thread between two of its requires, made
    /// deterministic by the channel wake (the previous missing piece). The
    /// worker requires the module, signals "ready" over a channel, parks on
    /// another; the main thread rewrites + reloads the module, then sends
    /// "go". The worker wakes, re-requires, and sees the NEW version — no
    /// timers, no polling, a pure message-passing sync point.
    #[test]
    fn live_worker_sees_reload_after_channel_wake() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_wake_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("rw_mod.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const fslib = require('fs');
                channel.create("rw_ready");
                channel.create("rw_go");
                const p = spawn(function () {{
                    const ready = channel.get("rw_ready");
                    const go = channel.get("rw_go");
                    return (async function () {{
                        const a = require('./rw_mod.ajs').get();
                        ready.send("ready");
                        await go.recv();
                        const b = require('./rw_mod.ajs').get();
                        return [a, b];
                    }})();
                }});
                // Wait until the worker has required v1 (channel wake!), then
                // hot-reload the module while the worker is parked.
                await channel.get("rw_ready").recv();
                fslib.writeFileSync('{}', 'export function get() {{ return 2; }}');
                print('reloaded:' + reload('./rw_mod.ajs'));
                channel.get("rw_go").send("go");
                print((await p).join(","));
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\n1,2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Named channels resolve on every VM: `get` on a missing name throws a
    /// loud, catchable error (no silent undefined).
    #[test]
    fn named_channel_missing_name_throws() {
        let src = r#"
            try {
                channel.get("never_created");
                print("no error");
            } catch (e) {
                print("missing:" + (("" + e).indexOf("not found") >= 0));
            }
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "missing:true");
    }

    /// corpus previously dodged. Expected values verified against Node.
    #[test]
    fn array_join_push_and_date_now_match_node() {
        let src = r#"
            let xlog = [];
            let l1 = xlog.push(1);
            let l2 = xlog.push(2, 3);
            xlog.push("four");
            print(l1, l2, xlog.length, xlog.join(","), xlog.join("-"), "[" + xlog.join() + "]");
            print("[" + [].join(",") + "]", [null, undefined, 5].join("|"), [[1, 2], [3]].join("+"));
            let t = Date.now();
            let t2 = Date.now();
            print(t <= t2);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "1 3 4 1,2,3,four 1-2-3-four [1,2,3,four] [] ||5 1,2+3 true");
    }

    /// The full `Array.prototype` / `String.prototype` round-out: pop, shift,
    /// unshift (return values + mutation), slice (positive/negative indices),
    /// concat (arrays flatten, scalars append), indexOf/includes (incl. NaN
    /// behavior), map/forEach (callbacks receive element/index/array),
    /// charAt (out-of-bounds and negative → ""), substring (swap/clamp),
    /// split (sep, empty sep, missing sep), toUpperCase. Expected values
    /// verified against Node.
    #[test]
    fn array_string_prototype_roundout_matches_node() {
        let src = r#"
            let a = [1, 2, 3, 4, 5];
            print(a.pop(), a.shift(), a.unshift(9, 8));
            print(a.slice(1, 3).join(","), [1, 2].concat([3, 4], 5).join(","));
            print([1, 2, 3, 2].indexOf(2, 1), [NaN, 1].includes(NaN), [NaN, 1].indexOf(NaN));
            let d = [1, 2, 3].map(function (x) { return x * 2; });
            let s = 0;
            [1, 2, 3].forEach(function (x) { s += x; });
            print(d.join("-"), s);
            print("hello".charAt(1), "hi".charAt(5), "hi".charAt(-1), "hello".substring(1, 3), "hello".substring(3, 1), "a,b,c".split(",").join("|"), "abc".split("").join("-"), "heLLo".toUpperCase(), "x".split().length);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "5 1 5 8,2 1,2,3,4,5 1 true -1 2-4-6 6 e   el el a|b|c a-b-c HELLO 1");
    }

    /// The second prototype round-out: String.replace ($ patterns, function
    /// replacement, empty pattern, missing replacement), trim (with the BOM
    /// edge — verified live against Node with a raw \uFEFF byte, since the
    /// lexer doesn't decode escape sequences), indexOf/lastIndexOf (NaN,
    /// +-Infinity, clamping, empty needle), Array.sort (default lexicographic
    /// order, comparator function, undefined sorts last) and reverse
    /// (in-place, returns the same array). Expected values verified against
    /// Node.
    #[test]
    fn replace_trim_indexof_sort_reverse_match_node() {
        let src = r#"
            print("hello world".replace("o", "[$&][$`][$'][$$]"));
            print("abc".replace("b", function (m, off, s) { return m + "@" + off; }));
            print("abc".replace("x", "Z"), "abc".replace("", "X"), "abc".replace("b", "$1"), "abc".replace("b"));
            print(" a b ".trim() + "|");
            print("abcabc".indexOf("b"), "abcabc".indexOf("b", 2), "abc".indexOf(""), "abc".indexOf("", 5), "abc".indexOf("b", NaN), "abcabc".indexOf("b", Infinity));
            print("abcabc".lastIndexOf("b"), "abcabc".lastIndexOf("b", 3), "abc".lastIndexOf("b", -1), "abc".lastIndexOf("b", NaN), "abc".lastIndexOf("", 2), "abc".lastIndexOf("", -1));
            print([10, 9, 100, 1].sort().join(","));
            print([undefined, null, 3, 1].sort().join(","));
            print([10, 2, NaN].sort().join(","));
            print([3, 1, 4, 1, 5].sort(function (a, b) { return a - b; }).join(","));
            let r = [5, 1, 4];
            let r2 = r.reverse();
            print(r.join(","), r2.join(","), r === r2);
            print("hello".indexOf("l"), "hello".lastIndexOf("l"));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "hell[o][hell][ world][$] world ab@1c abc Xabc a$1c aundefinedc a b| \
             1 4 0 3 1 -1 4 1 -1 1 2 0 1,10,100,9 1,3,, 10,2,NaN 1,1,3,4,5 \
             4,1,5 4,1,5 true 2 3"
        );
    }

    /// The third prototype round-out: String.slice/substr (negative and
    /// clamped indices), includes/startsWith/endsWith (NaN, Infinity, empty
    /// search, endPosition), padStart/padEnd (default pad, truncation,
    /// fractional target), and Array find/findIndex/filter/some/every
    /// (truthiness callbacks, empty-array results). Expected values verified
    /// against Node.
    #[test]
    fn slice_substr_includes_pad_find_filter_match_node() {
        let src = r#"
            print("hello world".slice(3), "hello world".slice(-3), "hello world".slice(1, -1), "hello".slice(5, 2), "hello".slice(NaN, 3), "hello".slice(-99, 3));
            print("hello".substr(2), "hello".substr(-3), "hello".substr(1, 2), "hello".substr(3, 99), "hello".substr(-99), "hello".substr(1, -1));
            print("hello".includes("ll"), "hello".includes("", 99), "hello".includes("x", -5), "hello".includes("h", Infinity));
            print("hello".startsWith("he"), "hello".startsWith("l", 2), "hello".startsWith("", 99), "hello".startsWith("x"));
            print("hello".endsWith("lo"), "hello".endsWith("l", 3), "hello".endsWith("", 0), "hello".endsWith("lo", NaN));
            print("5".padStart(3), "5".padStart(3, "0"), "5".padStart(4, "ab"), "abc".padEnd(5, "-"), "".padStart(2), "5".padStart(2.7, "x"));
            print([1, 2, 3].find(function (x) { return x > 1; }), [1, 2, 3].find(function (x) { return x > 9; }));
            print([1, 2, 3].findIndex(function (x) { return x > 1; }), [1, 2, 3].findIndex(function (x) { return x > 9; }));
            print([1, 2, 3, 4].filter(function (x) { return x % 2 == 0; }).join(","), [1, 2].filter(function (x) { return x > 5; }).length);
            print([1, 2, 3].some(function (x) { return x == 2; }), [].some(function (x) { return true; }));
            print([1, 2, 3].every(function (x) { return x > 0; }), [].every(function (x) { return false; }));
            print([0, null, undefined, 2].find(function (x) { return x; }), [0, 1].findIndex(function (x) { return x; }));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "lo world rld ello worl  hel hel llo llo el lo hello  true true false false \
             true true true false true true true false   5 005 aba5 abc--    x5 2 undefined \
             1 -1 2,4 0 true false true true 2 1"
        );
    }

    /// The Math/Number globals: floor/ceil/round (round's -0 for negatives in
    /// [-0.5, 0)), abs/sqrt/pow (incl. NaN and -0), min/max (NaN propagation,
    /// -0/+0 selection, empty-arg ±Infinity), parseInt (hex auto-detect,
    /// radix validation, no octal for "010"), parseFloat (decimal prefixes,
    /// Infinity literal), Number.isNaN (type-strict), and Math.random range.
    /// Expected values verified against Node.
    #[test]
    fn math_and_number_globals_match_node() {
        let src = r#"
            print(Math.floor(2.7), Math.floor(-0.5), Math.floor(-0), Math.floor(NaN), Math.floor(Infinity));
            print(Math.ceil(0.1), Math.ceil(-0.5), Math.ceil(-1.2), Math.ceil(5));
            print(Math.round(0.5), Math.round(-0.5), Math.round(-1.5), Math.round(0.4), Math.round(-0.4));
            print(Math.abs(-5), Math.abs(-0), Math.abs(-Infinity), Math.abs(NaN));
            print(Math.sqrt(4), Math.sqrt(-1), Math.sqrt(2));
            print(Math.pow(2, 3), Math.pow(-1, 0.5), Math.pow(2, -1), Math.pow(0, 0));
            print(Math.min(), Math.max(), Math.min(3, 1, 2), Math.min(NaN, 5), Math.min(-0, 0), Math.max(-0, 0));
            print(parseInt("42"), parseInt("0x10"), parseInt("0x10", 10), parseInt("101", 2), parseInt("zz", 36), parseInt(""), parseInt("3.14"), parseInt("   -0x10"));
            print(parseInt("010"), parseInt("0b101"), parseInt("ff", 16), parseInt("10", 1), parseInt("10", 37));
            print(parseFloat("3.14abc"), parseFloat("  -1.5e2"), parseFloat("0x10"), parseFloat("Infinity"), parseFloat(""), parseFloat(".5"), parseFloat("5."), parseFloat("abc"));
            print(Number.isNaN(NaN), Number.isNaN("abc"), Number.isNaN(5), Number.parseInt("10", 2), Number.parseFloat("2.5"));
            print(isNaN("abc"), isNaN(5));
            let r = Math.random();
            print(r >= 0 && r < 1);
            print(Math.floor(5) === 5, Math.round(-0.1) === 0, Math.round(-0.1), Math.floor(2.999), Math.ceil(-2.999));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "2 -1 -0 NaN Infinity 1 -0 -1 5 1 -0 -1 0 -0 5 0 Infinity NaN 2 NaN \
             1.4142135623730951 8 NaN 0.5 1 Infinity -Infinity 1 NaN -0 0 42 16 0 5 \
             1295 NaN 3 -16 10 0 255 NaN NaN 3.14 -150 0 Infinity NaN 0.5 5 NaN \
             true false false 2 2.5 true false true true true -0 2 -2"
        );
    }

    /// Map/Set: `new Map()`/`new Set()` (native constructors), SameValueZero
    /// keys (`NaN` finds `NaN`, `-0`/`+0` share a slot, `1` and `1.0` are one
    /// key, objects by identity), `get`/`set`/`has`/`delete`/`clear`/`size`
    /// (and Set's `add`), `instanceof` against the native constructor, and
    /// object-key churn that forces young→old promotion and old-gen sweeps
    /// (the entry table is rebuilt against remapped addresses). Expected
    /// values verified against Node.
    #[test]
    fn map_set_match_node() {
        let src = r#"
            let m = new Map();
            m.set("a", 1); m.set("b", 2);
            print(m.get("a"), m.get("zz"), m.has("a"), m.size);
            print(m.delete("a"), m.has("a"), m.size);
            m.clear(); print(m.size);
            m.set(1, "one"); print(m.get(1.0), m.get(2));
            m.set(NaN, "nan"); print(m.has(NaN), m.get(NaN));
            m.set(-0, "z"); print(m.get(0));
            let o = { x: 1 }; m.set(o, "obj");
            print(m.get(o), m.get({ x: 1 }), m.size);
            print(m instanceof Map, typeof Map, Map.prototype !== undefined);
            let s = new Set();
            s.add(5); s.add(5.0); s.add("s");
            print(s.size, s.has(5), s.has("s"), s.has(6));
            print(s.delete(5), s.size, s.has(5));
            print(s instanceof Set);
            let m2 = new Map();
            for (let i = 0; i < 5000; i++) { let k = { i: i }; m2.set(k, i); if (i % 3 === 0) m2.delete(k); }
            print(m2.size);
            let sk = []; let ms = new Map();
            for (let i = 0; i < 5000; i++) { let k = "k" + (i % 500); if (i % 4 === 0) sk.push(k); ms.set(k, i); }
            let ssum = 0;
            for (let j = 0; j < sk.length; j++) { let v = ms.get(sk[j]); if (v !== undefined) ssum += v; }
            print(sk.length, ms.size, ssum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "1 undefined true 2 true false 1 0 one undefined true nan z obj undefined 4 \
             true function true 2 true true false true 1 false true 3333 1250 500 5935000"
        );
    }

    /// Map/Set iteration: `keys()`/`values()`/`entries()` return array
    /// snapshots in insertion order (the engine has no iterator protocol),
    /// `forEach` walks live with `(value, key, map)` args and the map as the
    /// third arg, re-setting a key keeps its position while delete+re-add
    /// moves it to the end, Set entries are `[v, v]` pairs, and a 1000-key
    /// churn with 333 deletes exercises the tombstone compaction without
    /// disturbing order. Expected values verified against Node.
    #[test]
    fn map_set_iteration_match_node() {
        let src = r#"
            let m = new Map();
            m.set("b", 2); m.set("a", 1); m.set("c", 3);
            print([...m.keys()].join(","));
            print([...m.values()].join(","));
            print([...m.entries()].map(e => e.join(":")).join("|"));
            m.set("a", 99);
            print([...m.keys()].join(","), [...m.values()].join(","));
            m.delete("b");
            print([...m.keys()].join(","), m.size);
            m.set("b", 7);
            print([...m.keys()].join(","));
            let acc = [];
            m.forEach((v, k) => acc.push(k + "=" + v));
            print(acc.join(";"));
            let sacc = [];
            m.forEach(function (v, k, mm) { sacc.push(k + ":" + v + ":" + (mm === m)); });
            print(sacc.join(";"));
            let s = new Set();
            s.add("x"); s.add("y"); s.add("z");
            print([...s.keys()].join(","), [...s.values()].join(","));
            print([...s.entries()].map(e => e.join(":")).join("|"));
            let acc2 = [];
            s.forEach((v, k) => acc2.push(k + "=" + v));
            print(acc2.join(";"));
            s.delete("y");
            s.add("y");
            print([...s.keys()].join(","));
            let big = new Map();
            for (let i = 0; i < 1000; i++) big.set("k" + i, i);
            for (let i = 0; i < 1000; i += 3) big.delete("k" + i);
            print(big.size, [...big.keys()].slice(0, 6).join(","), [...big.keys()][big.size - 1]);
            let sum = 0;
            big.forEach((v, k) => { sum += v; });
            print(sum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "b,a,c 2,1,3 b:2|a:1|c:3 b,a,c 2,99,3 a,c 2 a,c,b a=99;c=3;b=7 \
             a:99:true;c:3:true;b:7:true x,y,z x,y,z x:x|y:y|z:z x=x;y=y;z=z x,z,y \
             666 k1,k2,k4,k5,k7,k8 k998 332667"
        );
    }

    /// The arena-backed entry table across a full GC lifecycle. Run 1 seeds
    /// a global Map with 30k churned object keys + string keys (table growth
    /// rehashes into fresh young slot regions) and run's end promotes the
    /// map, its keys, and its slot region into the old generation. Run 2
    /// then probes the *promoted* table: object keys must still resolve by
    /// identity (the table was rebuilt against remapped addresses), SameValue
    /// Zero keys (NaN/-0/1 vs 1.0) must hit, insertion order must survive,
    /// and delete+re-add must still move the key to the end. The big old-gen
    /// region also forces major sweeps whose mark must keep the slot region
    /// alive. Expected values verified against Node on the concatenated
    /// script.
    #[test]
    fn map_arena_table_survives_promotion_and_sweep() {
        let run1 = Compiler::compile_source_with_mode(
            r#"
            let keys = [];
            let m = new Map();
            for (let i = 0; i < 30000; i++) {
                let k = { i: i };
                if (i % 5 === 0) keys.push(k);
                m.set(k, i);
                if (i % 3 === 0) m.delete(k);
                if (i % 11 === 0) m.set("s" + (i % 500), i);
            }
            m.set(NaN, "nan");
            m.set(-0, "z");
            m.set(1, "one");
            // Old-gen garbage for the sweep: a big array promoted at run 1's
            // boundary, then dropped in run 2 — the mark must not reach it.
            scratch = [];
            for (let i = 0; i < 5000; i++) scratch.push({ x: i });
            print("r1", m.size, keys.length);
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(run1);
        vm.run();
        let run2 = Compiler::compile_source_with_mode(
            r#"
            let sum = 0, missing = 0;
            for (let j = 0; j < keys.length; j++) {
                let v = m.get(keys[j]);
                if (v === undefined) missing++;
                else sum += v;
            }
            print("r2", m.size, missing, sum);
            print(m.get(NaN), m.get(0), m.get(-0), m.get(1.0), m.get("s7"));
            let ks = [...m.keys()];
            print("order", ks.length, ks.length === m.size);
            m.delete(keys[0]);
            m.set(keys[0], 999);
            let ks2 = [...m.keys()];
            print("readd", ks2.length, ks2[ks2.length - 1] === keys[0], m.get(keys[0]));
            let cnt = 0;
            m.forEach((v, k) => { cnt++; });
            print("cnt", cnt);
            // Drop the old-gen scratch: its 5000 boxes become unreachable and
            // the next major sweep must reclaim them.
            scratch = null;
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run2);
        vm.run();
        // Drive the incremental major GC to completion: enough unit boundaries
        // for the budgeted mark to drain and the old-generation sweep to run
        // WHILE the map (and its arena slot region) is live — the sweep must
        // keep the region, or the next lookup reads freed arena memory.
        for _ in 0..400 {
            vm.promote_generation();
        }
        // The sweep reclaimed the churned old-gen garbage.
        assert!(
            vm.heap.free_bytes() > 0,
            "expected the major sweep to have reclaimed dead old-gen space"
        );
        let run3 = Compiler::compile_source_with_mode(
            r#"
            let sum = 0, missing = 0;
            for (let j = 0; j < keys.length; j++) {
                let v = m.get(keys[j]);
                if (v === undefined) missing++;
                else sum += v;
            }
            let cnt = 0;
            m.forEach((v, k) => { cnt++; });
            print("r3", m.size, missing, sum, cnt);
            print(m.get(NaN), m.get(0), m.get(1.0), m.get("s7"));
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run3);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "r1 20503 6000\nr2 20503 2000 60000000\n\
             nan z z one 29007\n\
             order 20503 true\n\
             readd 20504 true 999\n\
             cnt 20504\n\
             r3 20504 1999 60000999 20504\n\
             nan z one 29007"
        );
    }

    /// JSON.stringify/parse: insertion-order keys (object literals, parse
    /// round-trips), the `space` pretty-print arg (number/string/tab, 0 →
    /// compact), NaN/Infinity/-0 → null/0, undefined/function collapsing in
    /// arrays (null) and objects (omitted), `\uXXXX`/surrogate escapes in
    /// parse, -0 preserved, cycles and syntax errors caught by try/catch
    /// through the native throw hook. Expected values verified against Node.
    #[test]
    fn json_stringify_parse_match_node() {
        let src = r###"
            print(JSON.stringify({ a: 1, b: 2 }), JSON.stringify({ b: 2, a: 1 }));
            print(JSON.stringify([1, "x", true, null, 2.5]));
            print(JSON.stringify(NaN), JSON.stringify(Infinity), JSON.stringify(-0), JSON.stringify(undefined));
            print(JSON.stringify([1, undefined, function () {}, 3]));
            print(JSON.stringify({ a: 1, b: undefined, c: function () {}, d: 4 }));
            print(JSON.stringify({ a: 1, b: [1, 2] }, null, 0));
            print(JSON.parse("{\"a\":1,\"b\":[true,null,\"x\"]}").a, JSON.parse("{\"a\":1,\"b\":[true,null,\"x\"]}").b.length);
            print(JSON.parse("42"), JSON.parse("-1.5"), JSON.parse("1e3"), JSON.parse("-0") === 0, 1 / JSON.parse("-0"));
            print(JSON.parse("\"caf\\u00e9\""), JSON.parse("\"\\uD83D\\uDE00\""));
            print(JSON.parse("[1,2,3]").join(","));
            let o = JSON.parse("{\"x\":1,\"y\":[2,3],\"z\":{\"w\":4}}");
            print(o.x, o.y.join(","), o.z.w);
            print(JSON.stringify(JSON.parse("{\"k\":1,\"j\":2}")));
            let cyc = {}; cyc.self = cyc;
            try { JSON.stringify(cyc); print("NO THROW"); } catch (e) { print("caught"); }
            try { JSON.parse("{bad}"); print("NO THROW2"); } catch (e) { print("caught2"); }
            print(JSON.stringify(""), JSON.stringify([]), JSON.stringify({}));
            print(JSON.stringify(JSON.parse(" {\"a\" : [1, 2] , \"b\": {\"c\": true}} ")));
        "###;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "{\"a\":1,\"b\":2} {\"b\":2,\"a\":1} [1,\"x\",true,null,2.5] null null 0 undefined \
             [1,null,null,3] {\"a\":1,\"d\":4} {\"a\":1,\"b\":[1,2]} 1 3 \
             42 -1.5 1000 true -Infinity café 😀 1,2,3 1 2,3 4 {\"k\":1,\"j\":2} \
             caught caught2 \"\" [] {} {\"a\":[1,2],\"b\":{\"c\":true}}"
        );
    }

    /// JSON.stringify's replacer argument: a key whitelist (applied to every
    /// object at every depth, arrays unfiltered, missing keys → empty object)
    /// and a function (called with `(key, value)` for root and each
    /// property/element — transformation, omission, root-undefined, array
    /// element → null, call order). Expected values verified against Node.
    #[test]
    fn json_replacer_matches_node() {
        let src = r###"
            print(JSON.stringify({ a: 1, b: 2, c: 3 }, ["a", "c"]));
            print(JSON.stringify({ a: 1, b: { c: 2, d: 3 } }, ["b", "c"]));
            print(JSON.stringify({ a: [1, 2, 3], b: 4 }, ["a"]));
            print(JSON.stringify({ a: 1, b: 2 }, function (k, v) { if (k === "b") { return undefined; } return v; }));
            print(JSON.stringify({ a: 1, b: "x" }, function (k, v) { return typeof v === "number" ? v * 2 : v; }));
            print(JSON.stringify({ a: 1 }, function () { return undefined; }));
            print(JSON.stringify([1, 2, 3], function (k, v) { if (typeof v === "number" && v > 1) { return undefined; } return v; }));
            let log = [];
            JSON.stringify({ a: 1, b: 2 }, function (k, v) { log.push(k); return v; });
            print(log.join(","));
            print(JSON.stringify({ a: 1, b: 2 }, ["missing"]));
            print(JSON.stringify({ a: { x: 1, y: 2 }, b: 3 }, ["a", "x"]));
        "###;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "{\"a\":1,\"c\":3} {\"b\":{\"c\":2}} {\"a\":[1,2,3]} {\"a\":1} \
             {\"a\":2,\"b\":\"x\"} undefined [1,null,null] ,a,b {} {\"a\":{\"x\":1}}"
        );
    }

    /// Number/string formatting round-out: toString(radix) with the
    /// V8 DoubleToRadixCString algorithm (verified on 535 (value, radix)
    /// pairs), toFixed/toPrecision rounding the exact binary value, string
    /// UTF-16 indexing (charCodeAt/codePointAt/length). Also covers the
    /// large-int dispatch fix — `9007199254740992.toString(2)` previously
    /// resolved to undefined because `Value::int` boxed it as a tagged misc.
    /// Expected values verified against Node.
    #[test]
    fn number_string_formatting_matches_node() {
        let src = r#"
            print((1234).toString(16), (255).toString(2), (0.5).toString(2), (0.3).toString(3));
            print((1.5).toString(3), (0.1).toString(16), (1e21).toString(16), (123.456).toString(16));
            print((-123.456).toString(16), (9007199254740991).toString(2), (9007199254740992).toString(2));
            print((1.5).toFixed(0), (1.005).toFixed(2), (2.5).toFixed(0), (0.1).toFixed(20));
            print((1e21).toFixed(2), (1e-7).toFixed(2), (0).toFixed(2));
            print((999.9).toPrecision(3), (0.0001).toPrecision(3), (123.456).toPrecision(5));
            print((1.5).toPrecision(2), (1e-7).toPrecision(1), (1e-7).toPrecision(3), (1234).toPrecision(2));
            print((1e25).toPrecision(30), (1e21).toPrecision(21), (0.1).toPrecision(17));
            print("hello".charCodeAt(1), "hello".charCodeAt(99), "héllo".charCodeAt(1), "hello".charCodeAt(-1));
            print("😀".codePointAt(0), "😀".charCodeAt(0), "😀".charCodeAt(1), "😀".length, "a😀b".length);
            print("a😀b".charCodeAt(1), "a😀b".codePointAt(1), (3.14).toString(), (1e21).toString());
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "4d2 11111111 0.1 0.0220022002200220022002200220022002 \
             1.111111111111111111111111111111112 0.1999999999999a 3635c9adc5dea00000 7b.74bc6a7ef9dc \
             -7b.74bc6a7ef9dc 11111111111111111111111111111111111111111111111111111 \
             100000000000000000000000000000000000000000000000000000 \
             2 1.00 3 0.10000000000000000555 \
             1e+21 0.00 0.00 \
             1.00e+3 0.000100 123.46 \
             1.5 1e-7 1.00e-7 1.2e+3 \
             10000000000000000905969664.0000 1.00000000000000000000e+21 0.10000000000000001 \
             101 NaN 233 NaN \
             128512 55357 56832 2 4 \
             55357 128512 3.14 1e+21"
        );
    }

    /// Per-slot SMI/number type feedback: the fused register-ALU, load/store
    /// and compare paths take raw i64/f64 lanes on slots whose feedback kind
    /// Parser hardening: sources that Node rejects are loud compile errors,
    /// not silent misparses — comma-less argument/element/field lists
    /// (`print(1 2)`, `[1 2]`, `{a: 1 b: 2}`), a number directly followed by
    /// an identifier (`0.toString`, which Node lexes as `0.` + identifier),
    /// unclosed parens/params (`(1`, `if (true {`), and the radix-literal
    /// member access that used to be rejected (`0x10.toString(16)` is VALID
    /// JS and must work). `.5` is a valid leading-dot number literal.
    #[test]
    fn parser_rejects_silent_misparses() {
        let err = |src: &str| {
            assert!(Compiler::compile_source(src).is_err(), "expected compile error for: {}", src);
        };
        err("print(1 2);");
        err("print(1 2 3);");
        err("print([1 2]);");
        err("print({a: 1 b: 2});");
        err("let [a b] = [1, 2];");
        err("let {a b} = {a: 1, b: 2};");
        err("print(0.toString(2));");
        err("0.toString;");
        err("print(1;");
        err("if (true { print(1); }");
        err("function f(a { return a; }");
        err("while (true { break; }");
        err("let x = (1; print(x);");
        err("print(0x1.5);");

        // Valid forms these checks must NOT break (newlines are implicit
        // statement separators in the engine, matching its test corpus).
        let ok = |src: &str, expect: &str| {
            let program = Compiler::compile_source(src).expect("compile");
            let (mut vm, out) = Vm::with_output(program);
            vm.run();
            assert_eq!(out.lock().unwrap().join("\n"), expect, "for: {}", src);
        };
        ok("const sq = x => x * x\nprint(sq(5))", "25");
        ok("print(0x10.toString(16), 0b101.toString(2), 0o17.toString(8))", "10 101 17");
        ok("print(.5, .5e2, 0..toString(2), 1..toString()) ", "0.5 50 0 1");
        ok("print(1, 2, 3)", "1 2 3");
        ok("print([1, 2, 3], {a: 1, b: 2})", "[1, 2, 3] [object Object]");
        ok("let [h, , ...t] = [1, 2, 3, 4]; print(h, t.length)", "1 2");
        ok("function f(a, b) { return a + b; } print(f(1, 2))", "3");
        ok("print((1 + 2) * 3, (1, 2, 3))", "9 3");
        // `instanceof` is a real operator (class work), not a silent misparse.
        ok("class A {} let a = new A(); print(a instanceof A, 1 instanceof Number, null instanceof A)", "true false false");
    }

    /// Undeclared identifiers are a ReferenceError at runtime, matching JS
    /// (`print(g)` throws; `typeof g` is "undefined" and does not). A
    /// declared-but-unassigned `let` is readable (undefined), and the error
    /// is catchable by try/catch.
    #[test]
    fn undeclared_globals_throw_reference_error() {
        let src = r#"
            print(typeof g);
            print(typeof (g));
            let g2 = 5; print(g2);
            print(typeof g2);
            let unassigned;
            print(unassigned);
            try { print(missing); } catch (e) { print("caught"); }
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "undefined undefined 5 number undefined caught");

        // An uncaught ReferenceError surfaces as the uncaught exception.
        let program = Compiler::compile_source("print(nope);").expect("compile");
        let (mut vm, out) = Vm::with_output(program);
        vm.run();
        assert!(out.lock().unwrap().is_empty());
        let e = vm.take_error().expect("ReferenceError recorded");
        assert!(format!("{:?}", e).contains("ReferenceError: nope is not defined"));
    }

    /// is INT or NUMBER. This exercises every fast lane plus the transitions
    /// that must fall back — cell slots (closure counter), params, int→f64
    /// flips (`n = n / 2`), BigInt overflow, and the f64 compare/mod edges.
    /// Expected values verified against Node.
    #[test]
    fn smi_feedback_lanes_match_node() {
        let src = r#"
            function counter() {
                let c = 0;
                return function () { c += 1; return c; };
            }
            function fib(n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); }
            function par(n) { let t = 0; for (let k = 0; k < n; k++) { t += k * 3; } return t; }
            function parf(n) { let t = 0; for (let k = 0; k < 10; k++) { t += n; n = n / 2; } return t; }
            let inc = counter();
            let cl = 0;
            for (let i = 0; i < 10; i++) { cl = inc(); }
            let x = 5;
            let xv = 0;
            for (let i = 0; i < 4; i++) {
                x = x / 2;      // f64 lane
                x = x * 2 + 1;  // f64 lane
                xv = xv + x;    // 6 + 7 + 8 + 9 (values verified in Node)
            }
            let f = fib(25);
            let p = 0;
            for (let j = 0; j < 50; j++) { p += par(100); }
            let pf = parf(1000);
            let big = 1;
            for (let i = 0; i < 60; i++) { big = big * 2; }
            let bigm = big % 7;
            let m0 = 5 % 0;
            let mneg = -9 % 3;
            let fa = 1.5, fb = 2.5;
            let c1 = fa < 2, c2 = fb !== 2.5, c3 = fa == 1.5;
            let j = 0, sum = 0;
            while (j < 5) { sum += j * 2 - 1; j += 1; }
            print(cl, xv, f, p, pf, bigm, m0, mneg, c1, c2, c3, sum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "10 30 75025 742500 1998.046875 1 NaN -0 true false true 15");
    }

    /// The growable string builder: `s = s + leaf` loops append into a
    /// shared buffer, yet ALIASED readers stay valid — `snap = s` mid-build
    /// must keep the prefix it captured, never see later appends — and the
    /// final builder survives promotion with its buffer in the old
    /// generation (contiguous, so reads need no flatten).
    #[test]
    fn string_builder_aliases_stay_valid_and_promote() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let big = null;
            let snap = null;
            function build() {
                let s = "";
                for (let i = 0; i < 10; i++) {
                    if (i === 4) { snap = s; }
                    s = s + "xy";
                }
                big = s;
                return s.length;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        use alloy_core::value::AString;
        let build = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "build")
            .map(|(v, _)| v.clone())
            .expect("build");
        vm.call_value(&build, &[]);
        vm.promote_generation();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after promote");
        let check = Compiler::compile_source_with_mode(
            "print(big.length, big, snap.length, snap, big[0], big[19]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "20 xyxyxyxyxyxyxyxyxyxy 8 xyxyxyxy x y");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
        let big_idx = vm.global_names.iter().position(|n| n == "big").expect("big");
        let addr = ((vm.globals[big_idx].bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(b.is_builder(), "loop-built string should be a growable builder");
        assert!(
            vm.heap.addr_in_old(b.bytes_ptr() as usize),
            "builder buffer must live in old, not young"
        );
        let snap_idx = vm.global_names.iter().position(|n| n == "snap").expect("snap");
        let saddr = ((vm.globals[snap_idx].bits() << 16) as i64 >> 16) as usize;
        let sb = unsafe { &*(saddr as *const AString) };
        assert_eq!(sb.len(), 8, "aliased snapshot must keep its prefix length");
    }

    /// Mixed concat through real JS: `s = s + i` with numbers, plus a
    /// string + object mix, must match Node's concatenation byte-for-byte.
    #[test]
    fn mixed_concat_matches_js_semantics() {
        let src = r#"
            let s = "";
            for (let i = 0; i < 50; i++) { s = s + i; }
            let o = { x: 1 };
            print(s, "pre" + 3.5 + true + null + undefined, "v=" + o);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "012345678910111213141516171819202122232425262728293031323334353637383940414243444546474849 pre3.5truenullundefined v=[object Object]"
        );
    }

    /// Rope + generations: a loop-built rope stored in a global survives
    /// promotion (the whole cons tree moves to old, iteratively), and the
    /// lazy flatten triggered by reading it afterwards allocates the flat
    /// bytes in the OLD generation — so they survive the young sweep instead
    /// of dangling.
    #[test]
    fn rope_promoted_then_flattened_lives_in_old_gen() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let big = null;
            function build() {
                let s = "";
                for (let i = 0; i < 2000; i++) { s = s + "xy"; }
                big = s;
                return big.length;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        use alloy_core::value::AString;
        let build = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "build")
            .map(|(v, _)| v.clone())
            .expect("build");
        // One unit: build the string (a growable builder — the small-start
        // fast path), promote it, reset young.
        vm.call_value(&build, &[]);
        vm.promote_generation();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after promote");
        // Reading the promoted builder is direct (already contiguous) and
        // its buffer must live in the OLD generation.
        let check = Compiler::compile_source_with_mode(
            "print(big.length, big[0], big[3999]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "4000 x y");
        assert_eq!(vm.heap_used_young(), 0, "must not leak young bytes");
        let big_idx = vm.global_names.iter().position(|n| n == "big").expect("big");
        let addr = ((vm.globals[big_idx].bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "big must not be a rope tree");
        assert!(
            vm.heap.addr_in_old(b.bytes_ptr() as usize),
            "builder bytes must live in old, not young"
        );
    }

    /// Rope + major GC: a rope stored in a global survives repeated
    /// second-generation sweeps (the mark records the whole cons subtree and
    /// every leaf byte region), and remains readable afterwards.
    #[test]
    fn rope_survives_major_gc_sweeps() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let rope_keep = "";
            function seed() {
                let s = "";
                for (let i = 0; i < 512; i++) { s = s + "abcdefghij"; }
                rope_keep = s;
            }
            function churn(n) {
                let t = "";
                for (let i = 0; i < 32; i++) { t = t + "junk!"; }
                return t;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        vm.major_threshold = 1 << 10;
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        for i in 0..500 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let check = Compiler::compile_source_with_mode(
            "print(rope_keep.length, rope_keep[0], rope_keep[5119]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = _sink.lock().unwrap().join("\n");
        assert_eq!(out, "5120 a j");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after sweeps");
    }

    /// The PRD headline: `import { f } from './x.py' as python`, allocate a
    /// Float32Array in the shared segment, hand its pointer to Python, and
    /// read the result back — zero-copy through the file-backed mmap both
    /// sides map. Skipped (passes trivially) when no `python` is installed.
    #[test]
    fn python_polyglot_sidecar() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_ai_model_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def processTensor(ptr):\n    total = 0.0\n    for i in range(8):\n        total += read_f32(ptr + i * 4)\n    return total\n\ndef scale(ptr, n):\n    return [read_f32(ptr + i * 4) * n for i in range(4)]\n\ndef boom():\n    raise ValueError('kaboom')\n",
        )
        .unwrap();
        // Forward slashes: the JS string lexer treats backslashes as escapes.
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ processTensor, scale, boom }} from '{}' as python;
            const buf = memory.allocateFloat32Array([1, 2, 3, 4, 5, 6, 7, 8]);
            (async () => {{
                const sum = await python.processTensor(buf.ptr);
                print("sum=" + sum);
                const scaled = await python.scale(buf.ptr, 2);
                print("scaled[2]=" + scaled[2]);
                try {{
                    await python.boom();
                    print("no-error");
                }} catch (e) {{
                    print("caught=" + e);
                }}
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "sum=36\nscaled[2]=6\ncaught=kaboom");
    }

    /// The in-process interpreter (`ALLOY_PYTHON_EMBED=1`): same PRD demo
    /// as [`python_polyglot_sidecar`], but the python runs in THIS process
    /// (GIL-guarded calls, no subprocess). This test is the embed suite — it
    /// self-skips unless the suite runs with embed enabled (the child-mode
    /// suite above covers the subprocess path).
    #[test]
    fn python_embed_roundtrip() {
        if !embed_mode() {
            eprintln!("skip: run the suite with ALLOY_PYTHON_EMBED=1 to exercise the in-process interpreter");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_embed_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def processTensor(ptr):\n    total = 0.0\n    for i in range(8):\n        total += read_f32(ptr + i * 4)\n    return total\n\ndef scale(ptr, n):\n    return [read_f32(ptr + i * 4) * n for i in range(4)]\n\ndef pick(a, b, c):\n    return b\n\ndef boom():\n    raise ValueError('kaboom')\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ processTensor, scale, pick, boom }} from '{}' as python;
            const buf = memory.allocateFloat32Array([1, 2, 3, 4, 5, 6, 7, 8]);
            (async () => {{
                const sum = await python.processTensor(buf.ptr);
                print("sum=" + sum);
                const scaled = await python.scale(buf.ptr, 2);
                print("scaled[2]=" + scaled[2]);
                print("pick=" + (await python.pick(1, "mid", 3)));
                print("pick2=" + (await python.pick(1, "other", 3)));
                try {{
                    await python.boom();
                    print("no-error");
                }} catch (e) {{
                    print("caught=" + e);
                }}
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "sum=36\nscaled[2]=6\npick=mid\npick2=other\ncaught=kaboom",
            "embed round-trip diverged from the child path: {}",
            out
        );
    }

    /// `finalize_interpreter()` refuses to run while any embed backend is
    /// alive — finalizing over a live module pointer would leave the
    /// interpreter with dangling references. This test holds its own live
    /// backend (so the guard is guaranteed to trip regardless of what other
    /// parallel tests do) and verifies the refusal; the real `Py_FinalizeEx`
    /// path is exercised by the subprocess CLI test (it is terminal for the
    /// process, so it can never run inside the shared test binary).
    #[test]
    fn finalize_guard_blocks_live_backends() {
        if !embed_mode() {
            eprintln!("skip: embed-only (run with ALLOY_PYTHON_EMBED=1)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_fin_guard_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ add }} from '{}' as python;
            (async () => {{ print("r=" + (await python.add(1, 2))); }})();
            "#,
            src
        ))
        .unwrap();
        // Other tests share the process-global interpreter and hold their
        // own backends, so only lower-bound assertions are sound here. The
        // precise teardown accounting (backends released → finalize
        // succeeds) is covered deterministically by the subprocess CLI test,
        // which cannot run inside this shared test binary.
        let baseline = crate::python_embed::live_backends();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        assert_eq!(sink.lock().unwrap().join("\n"), "r=3");
        // Our own backend is alive → the guard must refuse (and it must
        // refuse without touching the interpreter, so the process stays
        // usable for parallel tests).
        assert!(
            crate::python_embed::live_backends() >= baseline + 1,
            "expected at least our own live backend"
        );
        let err = crate::python_embed::finalize_interpreter()
            .expect_err("finalize must refuse while backends are live");
        assert!(
            err.contains("still alive"),
            "unexpected guard error: {}",
            err
        );
        assert!(
            !crate::python_embed::is_finalized(),
            "a refused finalize must not mark the interpreter finalized"
        );
        drop(vm);
        let _ = std::fs::remove_file(&py_path);
    }

    /// `sweepSegments()` is a global native: it reclaims orphaned segment
    /// files (dead pid, past the grace period) immediately — no 60s
    /// rate-limit wait — and returns `{ files, bytes }` describing what was
    /// reclaimed, so a long-running server can call it between requests and
    /// alert on `files > 0`. Safety rules are the same as the automatic
    /// sweep (a live process's segment is never touched).
    #[test]
    fn sweep_segments_native_reclaims_orphans_immediately() {
        // Advance the process-wide sweep rate-limit clock NOW (bypassing the
        // limit) so no concurrent VM's automatic sweep can run for the next
        // 60s and delete our orphan before the explicit sweepSegments()
        // call — making the assertion deterministic under parallel tests.
        // (The old warm-up VM only advanced the clock if its own automatic
        // sweep fired, which a concurrent test's earlier sweep can
        // rate-limit out.)
        alloy_core::shared_memory::sweep_segments_now();
        // A provably-dead, NON-RECYCLABLE pid for the fake orphan's
        // filename: far above any real pid range, so it is never a live
        // process (Unix: beyond pid_max → ESRCH; Windows: OpenProcess
        // fails) and the OS can never allocate it. A real dead pid from
        // spawn + reap can be recycled within milliseconds under parallel
        // load, making the file look live and the sweep skip it.
        const DEAD_PID: u32 = u32::MAX - 1;
        let dir = std::env::temp_dir();
        let orphan = dir.join(format!("alloy_shm_{}_999999999_0.tmp", DEAD_PID));
        std::fs::write(&orphan, b"orphan!").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        {
            // Write handle: setting file times needs FILE_WRITE_ATTRIBUTES,
            // which a read-only handle lacks on Windows.
            let f = std::fs::File::options().write(true).open(&orphan).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .expect("age the orphan past the grace period");
        }
        let src = r#"
            let r = sweepSegments();
            print("swept", r.files, r.bytes);
            print("keys", r.files >= 0, r.bytes >= 0);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" | ");
        assert!(out.contains("swept 1 7"), "sweepSegments must report the orphan, got: {out}");
        assert!(out.contains("keys true true"), "result object malformed, got: {out}");
        assert!(!orphan.exists(), "sweepSegments() must delete the orphan");
    }

    /// `spawn` runs a function as an isolated task on the event loop; tasks
    /// park on `await ch.recv()` and are woken by sends from other tasks.
    /// `spawn(fn)` runs `fn` on a worker thread in an isolated VM: the
    /// worker's captured state is a serialized copy (mutating `log` inside a
    /// task cannot touch the caller's `log`), and each task's result crosses
    /// back through the completion channel and settles as a promise.
    #[test]
    fn spawn_isolated_context_and_result_crossing() {
        let program = Compiler::compile_source(
            r#"
            async function main() {
                let log = "M";
                let r1 = await spawn(async () => {
                    // This log is a fresh copy in the worker's context.
                    return "A";
                });
                let r2 = await spawn(function () {
                    return "BC";
                });
                print("main=" + log + " p1=" + r1 + " p2=" + r2);
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "main=M p1=A p2=BC");
    }

    /// `await spawn(f)` yields f's result; a spawned task's state is isolated
    /// from the spawning scope (locals, not shared cells).
    #[test]
    fn spawn_await_result() {
        let program = Compiler::compile_source(
            r#"
            (async () => {
                const r = await spawn(() => 42);
                print("result=" + r);
            })();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "result=42");
    }

    /// The point of async python calls: a slow Python function must not
    /// freeze the event loop. A 10ms timer fires (and sets `t = 1`) while a
    /// 300ms python call is still in flight on its worker thread; the await
    /// resumes only afterwards with the timer's effect visible.
    #[test]
    fn python_call_does_not_block_event_loop() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_slow_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def slow(sec):\n    import time\n    time.sleep(sec)\n    return sec\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ slow }} from '{}' as python;
            let t = 0;
            setTimeout(() => {{ t = 1; print("timer"); }}, 10);
            (async () => {{
                const r = await python.slow(0.3);
                print("slow-done t=" + t);
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        let start = std::time::Instant::now();
        vm.run();
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        // The timer fired while the python call was in flight, so it prints
        // first and its side effect (t = 1) is visible when the await resumes.
        assert_eq!(out, "timer\nslow-done t=1");
        // Sanity: we waited for the slow call (~300ms) but the timer wasn't
        // delayed by it.
        assert!(elapsed.as_millis() >= 280, "ran too fast: {:?}", elapsed);
    }

    /// The HTTP-server shape: a handler awaits a python call, so after
    /// `call_value` suspends it, `drive_pending` must pump the loop until the
    /// promise settles and the handler resumes (the serve_http glue calls
    /// this before reading the response).
    #[test]
    fn python_call_from_handler_resolves() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_add_handler_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        // REPL mode so the top-level `handle` function becomes a global.
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ add }} from '{}' as python;
                async function handle(req, res) {{
                    const v = await python.add(2, 3);
                    print("handler-got=" + v);
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        // Simulate serve_http: invoke the handler, pump pending async work,
        // then (implicitly) the handler has resumed.
        vm.call_value(&handler, &[Value::undefined(), Value::undefined()]);
        vm.drive_pending();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "handler-got=5");
    }

    /// The worker queue: many sequential awaits all round-trip through the
    /// file's single dedicated worker thread (one `python_workers` entry, no
    /// per-call spawn), resolving in order with correct results.
    #[test]
    fn python_worker_queue_handles_many_calls() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_add_queue_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ add }} from '{}' as python;
            (async () => {{
                let s = 0;
                for (let i = 0; i < 25; i++) {{
                    s = await python.add(s, i);
                }}
                print("sum=" + s);
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        // Pool keys are canonical paths; resolve before the file is removed.
        let canon = vm.resolve_py_path(&src).unwrap_or(src.clone());
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        // 0 + 1 + ... + 24 = 300, resolved through 25 queued worker calls.
        assert_eq!(out, "sum=300");
        assert_eq!(vm.python_workers.len(), 1, "expected one worker per file");
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
        // Caveat 1 (lazy pool): 25 sequential calls never contended the single
        // child, so the pool must NOT have grown — exactly one child, with its
        // in-flight slot back to zero.
        let w = vm.python_workers.get(&canon).expect("worker pool");
        assert_eq!(w.senders.len(), 1, "lazy pool grew without contention");
        assert_eq!(w.busy.iter().sum::<usize>(), 0, "children left busy");
    }

    /// Caveat 2 (per-call timeout): a python function that never returns is
    /// killed at the deadline — the promise rejects with a timeout error —
    /// and the pool heals so the next call succeeds on the respawned child.
    #[test]
    fn python_call_times_out_and_worker_heals() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: per-call timeout kill is a child-mode feature");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_timeout_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def forever():\n    import time\n    time.sleep(3600)\ndef add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ forever, add }} from '{}' as python;
            (async () => {{
                let msg = "none";
                try {{
                    await python.forever();
                }} catch (e) {{
                    msg = "" + e;
                }}
                print("caught=" + msg);
                const healed = await python.add(2, 3);
                print("healed=" + healed);
            }})();
            "#,
            src
        ))
        .unwrap();
        // VM-scoped deadline: must NOT touch the process env var, or pools
        // that other tests spawn concurrently would inherit 400ms timeouts.
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_python_timeout(400);
        let start = std::time::Instant::now();
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert!(
            out.contains("caught=python call timed out"),
            "expected a timeout rejection, got: {}",
            out
        );
        assert!(out.contains("healed=5"), "worker did not heal: {}", out);
        // The whole run (400ms deadline + restart + a heal call) must finish
        // well under the 3600s the hung function would have taken.
        assert!(start.elapsed().as_secs() < 60, "test ran too long");
        assert_eq!(vm.python_inflight, 0);
    }

    /// A pure-python busy loop (no GIL-releasing C call) times out in BOTH
    /// backends: the child mode kills the process at the deadline, and the
    /// embed mode delivers the cooperative `KeyboardInterrupt` via
    /// `PyThreadState_SetAsyncExc` at the next bytecode boundary. Either way
    /// the promise rejects with a timeout error, the worker heals, and the
    /// next call succeeds. This is the embed-mode timeout path the
    /// `time.sleep` test above cannot cover (sleep releases the GIL, so the
    /// cooperative interrupt can't land until it returns).
    #[test]
    fn python_busy_loop_times_out_in_both_modes() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_busy_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def busy():\n    while True:\n        pass\ndef add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ busy, add }} from '{}' as python;
            (async () => {{
                let msg = "none";
                try {{
                    await python.busy();
                }} catch (e) {{
                    msg = "" + e;
                }}
                print("caught=" + msg);
                const healed = await python.add(2, 3);
                print("healed=" + healed);
            }})();
            "#,
            src
        ))
        .unwrap();
        // VM-scoped deadline: must NOT touch the process env var, or pools
        // that other tests spawn concurrently would inherit 400ms timeouts.
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_python_timeout(400);
        let start = std::time::Instant::now();
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert!(
            out.contains("caught=python call timed out"),
            "expected a timeout rejection, got: {}",
            out
        );
        assert!(out.contains("healed=5"), "worker did not heal: {}", out);
        assert!(start.elapsed().as_secs() < 60, "test ran too long");
        assert_eq!(vm.python_inflight, 0);
    }

    /// `reload('./x.py')` re-imports a python sidecar: the old child is
    /// killed and the next call runs the fresh file (Node-style cache
    /// invalidation for the python pillar). Old module references keep
    /// working because the natives route through the canonical path, which
    /// re-checks the generation on every call.
    #[test]
    fn reload_python_module_reimports() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_a_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                print('v1:' + await py.f());
                print('wrote:' + fslib.writeFileSync('{}', 'def f():\n    return 2\n'));
                print('reloaded:' + reload('{}'));
                print('v2:' + await py.f());
                print('reloaded-again:' + reload('{}'));
                print('v3:' + await py.f());
            }}
            main();
            "#,
            abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "v1:1\nwrote:true\nreloaded:true\nv2:2\nreloaded-again:true\nv3:2"
        );
        let _ = std::fs::remove_file(&py_path);
        assert_eq!(vm.python_workers.len(), 1);
    }

    /// The per-call generation check: a reload that lands WHILE a call is in
    /// flight aborts it — the sleeping child is killed — and re-runs the call
    /// on the freshly imported child, so the promise resolves with the new
    /// implementation instead of rejecting or settling the stale result.
    /// Deterministic: the first call sleeps 400ms; the write + reload happen
    /// microseconds later, long before it would have returned 1.
    #[test]
    fn reload_aborts_inflight_python_call_and_reruns() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_abort_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    import time\n    time.sleep(0.4)\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                const p = py.f();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                print('result:' + await p);
            }}
            main();
            "#,
            abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\nresult:2");
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// The headline cross-thread case: a LIVE spawn worker calls a python
    /// function, parks on a channel; the main thread rewrites the `.py` and
    /// reloads; the worker is woken by the channel send and re-imports the
    /// file on its next call — it sees the NEW result (2), not the stale
    /// child's (1). The main VM's own pool re-imports too. Requires `require`
    /// of a `.py`, which is how workers load python modules at all.
    #[test]
    fn python_reload_propagates_to_live_worker() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_b_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                channel.create("py_ready");
                channel.create("py_go");
                const p = spawn(function () {{
                    const ready = channel.get("py_ready");
                    const go = channel.get("py_go");
                    return (async function () {{
                        const m = require('{}');
                        const a = await m.f();
                        ready.send("ready");
                        await go.recv();
                        const m2 = require('{}');
                        const b = await m2.f();
                        return [a, b];
                    }})();
                }});
                print('main1:' + await py.f());
                // The worker has called f() once and parked on `go`.
                await channel.get("py_ready").recv();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                channel.get("py_go").send("go");
                print('worker:' + (await p).join(","));
                print('main2:' + await py.f());
            }}
            main();
            "#,
            abs, abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "main1:1\nreloaded:true\nworker:1,2\nmain2:2"
        );
        let _ = std::fs::remove_file(&py_path);
    }

    /// The cross-thread per-call check: a WORKER has a call in flight on its
    /// child when the MAIN thread reloads the `.py`. The worker's old child
    /// survives (it's a separate process from the main VM's pool) and finishes
    /// the sleep with the old code — but the worker's response drain compares
    /// the call's generation against the shared registry, sees it moved,
    /// tears down its own stale pool, and re-runs the call on a fresh child.
    /// The promise resolves with the new implementation (2), not the stale
    /// result (1), without the worker ever being told to reload explicitly.
    #[test]
    fn main_reload_reruns_worker_inflight_python_call() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_abortw_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.4)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                channel.create("abort_ready");
                const p = spawn(function () {{
                    const ready = channel.get("abort_ready");
                    return (async function () {{
                        const m = require('{}');
                        const a = m.f();      // in flight, sleeping 400ms
                        ready.send("in-flight");
                        return await a;       // reload lands mid-flight
                    }})();
                }});
                await channel.get("abort_ready").recv();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                print('worker:' + await p);
            }}
            main();
            "#,
            abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\nworker:2");
    }

    /// Coalescing: a BURST of reloads on the same `.py` while a call is in
    /// flight triggers exactly ONE pool rebuild and one re-run, not a re-run
    /// cascade. All four reloads land sub-ms apart — well inside the
    /// coalescing window and long before the call's response — so they fold
    /// into one burst: the first tears the pool down (the abort), the rest
    /// leave it alone. The single rebuild reads the file at rebuild time,
    /// i.e. the LAST write of the burst, and the re-run settles with it; the
    /// next call sees the same burst and serves the rebuilt pool with no
    /// further rebuild. Before coalescing, each reload bumped the version,
    /// so N reloads while the call (or its re-run) was in flight meant N
    /// rebuilds and N re-runs.
    #[test]
    fn burst_of_python_reloads_triggers_one_rebuild() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_burst_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.5)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                const p = py.f();       // in flight, sleeping 500ms
                // Burst: 4 rapid reloads, each also rewriting the file.
                for (let i = 0; i < 4; i++) {{
                    fslib.writeFileSync('{}', 'def f():\n    return ' + (2 + i) + '\n');
                    print('reloaded:' + reload('{}'));
                }}
                print('result:' + await p);   // re-run on the single rebuilt pool
                print('next:' + await py.f()); // same burst: no further rebuild
            }}
            main();
            "#,
            abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "reloaded:true\nreloaded:true\nreloaded:true\nreloaded:true\nresult:5\nnext:5"
        );
        assert_eq!(
            vm.python_rebuilds, 1,
            "a burst of reloads must rebuild the pool exactly once, not once per reload"
        );
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// The coalescing window is configurable via `ALLOY_PYTHON_RELOAD_MS`:
    /// widened past the default, two reloads spaced **beyond** the default
    /// 250ms window still fold into one burst (one pool rebuild). The same
    /// spacing at the default window would be two separate bursts — the
    /// second reload would tear the rebuilt pool down and the next call
    /// would rebuild again. The script spaces the reloads ~400ms apart (past
    /// the 250ms default, inside the 5000ms test window): the first reload
    /// aborts the in-flight call, the drain rebuilds once (reading v2), and
    /// the folded second reload neither bumps the version nor tears the pool
    /// down — exactly one rebuild, and the re-run resolves the state written
    /// before the first reload. The next call serves the same (un-rebuilt)
    /// pool, which is the documented coalescing tradeoff: a reload folded
    /// into the current burst is picked up by the next rebuild.
    #[test]
    fn widened_python_reload_window_folds_larger_spacing() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_win_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.15)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                const p = py.f();               // C1: v1, sleeps 150ms
                fslib.writeFileSync('{}', 'def f():\n    import time\n    time.sleep(0.15)\n    return 2\n');
                print('r1:' + reload('{}'));    // new burst: kills C1
                await sleep(400);               // drain rebuilds C2 (reads v2), re-runs
                fslib.writeFileSync('{}', 'def f():\n    import time\n    time.sleep(0.15)\n    return 3\n');
                print('r2:' + reload('{}'));    // ~400ms after r1: folded by the widened window
                print('result:' + await p);     // C2's re-run -> 2
                print('next:' + await py.f());  // same burst: same pool -> 2
            }}
            main();
            "#,
            abs, abs, abs, abs, abs
        ))
        .unwrap();
        let prev = std::env::var("ALLOY_PYTHON_RELOAD_MS").ok();
        std::env::set_var("ALLOY_PYTHON_RELOAD_MS", "5000");
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        match prev {
            Some(p) => std::env::set_var("ALLOY_PYTHON_RELOAD_MS", p),
            None => std::env::remove_var("ALLOY_PYTHON_RELOAD_MS"),
        }
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "r1:true\nr2:true\nresult:2\nnext:2");
        assert_eq!(
            vm.python_rebuilds, 1,
            "the widened window must fold the ~400ms-spaced reloads into one burst (one rebuild)"
        );
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// `reload` of a `.py` that exists but was never imported is a no-op
    /// (false), like the .ajs path; a missing file is false too.
    #[test]
    fn reload_unimported_python_is_false() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_c_{}.py", pid));
        std::fs::write(&py_path, "def g():\n    return 0\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let nope = dir.join(format!("alloy_pyrel_d_{}.py", pid));
        let nope_s = nope.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            print(reload('{}'));
            print(reload('{}'));
            "#,
            abs, nope_s
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "false\nfalse");
        let _ = std::fs::remove_file(&py_path);
    }

    /// The concurrent HTTP server: N requests sent at once, each handler
    /// awaiting a *different* slow python function (different files →
    /// different workers → parallel execution). Every response must be
    /// correct, and the total wall time must reflect parallel (~max, one
    /// slow call) rather than serialized (~sum, three slow calls).
    #[test]
    fn server_concurrent_await_python() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        // Three files so the three slow calls run on three workers in
        // parallel (same-file calls serialize on one worker).
        let mut py_paths = Vec::new();
        let mut imports = String::new();
        for i in 0..3usize {
            let p = dir.join(format!("alloy_test_slow_{}_{}.py", i, pid));
            std::fs::write(
                &p,
                format!("def f(sec):\n    import time\n    time.sleep(sec)\n    return {}\n", 100 + i * 100),
            )
            .unwrap();
            let s = p.to_string_lossy().replace('\\', "/");
            imports.push_str(&format!("import {{ f }} from '{}' as py{};\n", s, i));
            py_paths.push(p);
        }
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                {}
                async function handle(req, res) {{
                    const id = req.url;
                    const r = id == "/0" ? await py0.f(0.4) : id == "/1" ? await py1.f(0.4) : await py2.f(0.4);
                    res.send(id + ":" + r);
                }}
                "#,
                imports
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            // Look the handler up inside the thread: a function Value holds
            // Rc cells and is not Send, but the Vm itself is (unsafe impl).
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // N concurrent clients: connect + send together, then read together.
        let start = std::time::Instant::now();
        let clients: Vec<std::thread::JoinHandle<(usize, String)>> = (0..3usize)
            .map(|i| {
                let port = port;
                std::thread::spawn(move || {
                    let mut stream =
                        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
                    let req = format!("GET /{} HTTP/1.1\r\nHost: t\r\n\r\n", i);
                    stream.write_all(req.as_bytes()).unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    (i, String::from_utf8_lossy(&buf).to_string())
                })
            })
            .collect();
        let mut responses = Vec::new();
        for c in clients {
            responses.push(c.join().expect("client thread"));
        }
        let elapsed = start.elapsed();
        for p in &py_paths {
            let _ = std::fs::remove_file(p);
        }
        // Every response must carry its own request's marker from its own
        // python file (0→100, 1→200, 2→300).
        for (i, resp) in &responses {
            assert!(
                resp.contains(&format!("/{}:{}", i, 100 + i * 100)),
                "response for /{} was: {}",
                i,
                resp
            );
        }
        // Parallel: 3 × 0.4s python ≈ one 0.4s + overhead. Serialized
        // (requests served one at a time) would be ≥ 1.2s. 1.05s sits
        // between with margin on both sides.
        assert!(
            elapsed.as_millis() < 1050,
            "requests were serialized, not concurrent: {:?}",
            elapsed
        );
        // Stop the serve thread and join: the VM drops deterministically
        // (python children reaped, shared-segment file removed) instead of
        // lingering on a detached thread.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 1: same-file python calls no longer serialize. Two concurrent
    /// requests whose handlers both await a slow function from the *same*
    /// imported file run on the file's pool children in parallel: ~max(0.4s),
    /// not ~sum(0.8s).
    #[test]
    fn server_same_file_calls_run_in_parallel() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: same-file parallelism is a child-mode feature (GIL serializes)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_same_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def f(sec):\n    import time\n    time.sleep(sec)\n    return 42\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ f }} from '{}' as py;
                async function handle(req, res) {{
                    const r = await py.f(0.4);
                    res.send(req.url + ":" + r);
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // Warm-up pair: two overlapping requests trigger the pool's lazy
        // growth (child 0 is busy when the second lands), so the measured
        // pair below runs on a warm 2-child pool — the one-time child-spawn
        // latency stays out of the timing window.
        let warm: Vec<_> = (0..2)
            .map(|i| {
                let port = port;
                std::thread::spawn(move || http_client(port, &format!("/w{}", i)))
            })
            .collect();
        for t in warm {
            t.join().unwrap().expect("warm request");
        }
        let start = std::time::Instant::now();
        let clients: Vec<_> = (0..2)
            .map(|i| {
                let port = port;
                std::thread::spawn(move || http_client(port, &format!("/{}", i)))
            })
            .collect();
        let mut ok = true;
        for c in clients {
            match c.join() {
                Ok(Ok(resp)) => {
                    if !resp.contains("/0:42") && !resp.contains("/1:42") {
                        ok = false;
                    }
                }
                _ => ok = false,
            }
        }
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&py_path);
        assert!(ok, "one or more responses were wrong");
        // Parallel (warm 2-child pool): ~0.4s. Serialized on one child: ≥ 0.8s.
        assert!(
            elapsed.as_millis() < 650,
            "same-file calls were serialized: {:?}",
            elapsed
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Packed int arrays: the fast path must be invisible to semantics —
    /// reads, .length, holes, mixed-type escapes, `==` string coercion, and
    /// survival across a GC promotion all behave exactly like the general
    /// form. Also exercises the LoadLocalGetPropConst (`a.length`) and
    /// LoadLocalLocalGetIndex (`a[i]`) superinstructions in the loop.
    #[test]
    fn packed_int_array_semantics_and_fusions() {
        let src = r#"
            let keep = null;
            function seed() {
                keep = [1, 2, 3];
                // Escape to mixed on a non-int write; the earlier ints must
                // stay visible.
                keep[3] = "four";
            }
            seed();
            let a = [1, 2, 3];
            print("len=" + a.length);
            print("idx=" + a[1]);
            print("hole=" + a[7]);
            a[5] = 42;
            print("extended=" + a.length + ":" + a[4] + ":" + a[5]);
            let s = 0;
            for (let i = 0; i < a.length; i++) {
                let v = a[i];
                if (v !== undefined) { s += v; }
            }
            print("sum=" + s);
            // == coercion on a packed array comma-joins like JS.
            print("eq=" + ([1, 2] == "1,2"));
            print("mixed=" + keep[0] + ":" + keep[3]);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "len=3\nidx=2\nhole=undefined\nextended=6:undefined:42\nsum=48\neq=true\nmixed=1:four"
        );
    }

    /// Caveat 1 (lazy pool growth under contention): a second call that
    /// lands while the first is still in flight must grow the file's pool to
    /// a second child, and both calls complete in parallel (each response
    /// correct).
    #[test]
    fn python_pool_grows_lazily_under_contention() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: pool growth is a child-mode feature (embed caps at one child)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_grow_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def f(sec, tag):\n    import time\n    time.sleep(sec)\n    return tag\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as python;
            (async () => {{
                const p1 = python.f(0.3, 1);
                const p2 = python.f(0.3, 2);
                print("a=" + (await p1) + " b=" + (await p2));
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        // Pool keys are canonical paths; resolve before the file is removed.
        let canon = vm.resolve_py_path(&src).unwrap_or(src.clone());
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "a=1 b=2");
        let w = vm.python_workers.get(&canon).expect("worker pool");
        // Both calls were in flight at once, so the pool grew to its cap of 2.
        assert_eq!(w.senders.len(), 2, "pool did not grow under contention");
        assert_eq!(w.busy.iter().sum::<usize>(), 0, "children left busy");
    }

    /// Caveat 3: a handler whose python call rejects responds 500 with the
    /// rejection reason, instead of a silent default body.
    #[test]
    fn server_handler_error_returns_500() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_boom_{}.py", std::process::id()));
        std::fs::write(&py_path, "def boom():\n    raise ValueError('kaboom from python')\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ boom }} from '{}' as py;
                async function handle(req, res) {{
                    await py.boom();
                    res.send("never-reached");
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let resp = http_client(port, "/").expect("request");
        let _ = std::fs::remove_file(&py_path);
        assert!(
            resp.starts_with("HTTP/1.1 500"),
            "expected 500, got: {}",
            resp.lines().next().unwrap_or("")
        );
        assert!(resp.contains("kaboom from python"), "missing reason in: {}", resp);
        assert!(!resp.contains("never-reached"), "handler continued after rejection");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 3: a *synchronous* throw inside a handler must also become a
    /// 500 (not abort the VM as an uncaught top-level throw), and the server
    /// must keep serving afterwards.
    #[test]
    fn server_sync_handler_throw_returns_500_and_keeps_serving() {
        let program = Compiler::compile_source_with_mode(
            r#"
            function handle(req, res) {
                if (req.url === "/boom") { throw "sync-boom"; }
                res.send("alive");
            }
            "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let boom = http_client(port, "/boom").expect("boom request");
        assert!(
            boom.starts_with("HTTP/1.1 500"),
            "expected 500, got: {}",
            boom.lines().next().unwrap_or("")
        );
        assert!(boom.contains("sync-boom"), "missing reason in: {}", boom);
        // The VM must not have aborted: the next request still gets served.
        let alive = http_client(port, "/ok").expect("alive request");
        assert!(alive.contains("alive"), "server died after the throw: {}", alive);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 2: a client that connects and stalls mid-request does not block
    /// the loop. While client 1 sits silent, client 2 must be accepted, read,
    /// and served promptly.
    #[test]
    fn server_stalled_client_does_not_block_others() {
        let program = Compiler::compile_source_with_mode(
            r#"
            function handle(req, res) { res.send("fast"); }
            "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // Client 1: connect and send nothing — under the old blocking read,
        // this would stall serve_loop forever and starve every other client.
        let stalled = std::net::TcpStream::connect(("127.0.0.1", port)).expect("stalled connect");
        let _ = stalled.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        // Give the server time to accept (and, in the old code, block on) it.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let start = std::time::Instant::now();
        let resp = http_client(port, "/").expect("fast client request");
        let elapsed = start.elapsed();
        assert!(resp.contains("fast"), "fast client was not served: {}", resp);
        assert!(
            elapsed.as_millis() < 1000,
            "fast client stalled behind the silent one: {:?}",
            elapsed
        );
        drop(stalled);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

}

#[cfg(test)]
mod dbg_forof {
    use crate::compiler::Compiler;
    #[test]
    fn dbg_forof_compile() {
        let src = "let s = 0\nfor (const v of [1, 2, 3, 4]) { s = s + v }\nprint(s)";
        match Compiler::compile_source(src) {
            Ok(_) => eprintln!("compile_source(false): OK"),
            Err(e) => eprintln!("compile_source(false): ERR {:?}", e),
        }
        match Compiler::compile_source_with_mode(src, true, false) {
            Ok(_) => eprintln!("compile_source(true): OK"),
            Err(e) => eprintln!("compile_source(true): ERR {:?}", e),
        }
        // also try with semicolons after the statements
        let src2 = "let s = 0; for (const v of [1, 2, 3, 4]) { s = s + v } print(s);";
        match Compiler::compile_source(src2) {
            Ok(_) => eprintln!("semicolon version: OK"),
            Err(e) => eprintln!("semicolon version: ERR {:?}", e),
        }
    }
}

