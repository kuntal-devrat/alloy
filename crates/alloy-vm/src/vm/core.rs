use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use hashbrown::HashMap;

use alloy_core::arena::ChunkedArena;
use alloy_core::heap::{ArenaHeap, HeapGuard};
use alloy_core::regex::RegexCompiled;
use alloy_core::shared_memory::SidecarMemory;
use alloy_core::value::{
    MarkState, PromiseState, PromiseStatus, RcDirtyRef, Value, VmHost, WakeHandle,
};
use crate::bytecode::Program;
use crate::opcode::Opcode;

use super::builtins::{error_ctor_map, is_error_name, seed_global};
use super::cache::{CallIcEntry, IcPoly, IC_SLOTS};
use super::generator::GeneratorState;
use super::modules::{ModuleGen, SharedModuleRegistry, SharedPyRegistry};
use super::ops_async::{Continuation, Handler, Microtask, ThrowResult, Timer};
use super::python::{InflightPyCall, PythonWorker};
use super::stack::{CallFrame, OperandStack};

// TEMP profiling: ALLOY_OP_HIST=1 enables an opcode execution histogram.
static OP_HIST: std::sync::OnceLock<Option<std::sync::Mutex<[u64; 256]>>> =
    std::sync::OnceLock::new();
pub(crate) fn op_hist() -> Option<&'static std::sync::Mutex<[u64; 256]>> {
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

#[inline]
pub(crate) fn unwrap_cell(v: Value) -> Value {
    match v.as_cell() {
        Some(c) => {
            if let Ok(g) = c.try_borrow() {
                g.clone()
            } else {
                c.borrow().clone()
            }
        }
        None => v,
    }
}

pub(crate) const STACK_SIZE: usize = 16384;
pub(crate) const MAJOR_THRESHOLD_INIT: usize = 1 << 20;
pub(crate) const MAJOR_THRESHOLD_MIN: usize = 1 << 16;
pub(crate) const MAJOR_THRESHOLD_MAX: usize = 1 << 24;
pub(crate) const FRAME_BUDGET: usize = 512;
pub(crate) const SHARED_MEMORY_CAPACITY: usize = 1 << 20;
pub(crate) const MAX_COMPILED_MODULES: usize = 4096;
pub(crate) const MAX_PY_TRACKED_MODULES: usize = 4096;

pub(crate) fn shared_memory_capacity() -> usize {
    std::env::var("ALLOY_SHM_CAP")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(SHARED_MEMORY_CAPACITY)
        .max(SHARED_MEMORY_CAPACITY)
}
pub(crate) const MAX_CALL_DEPTH: usize = 512;
pub(crate) const PYTHON_POOL_SIZE: usize = 2;
pub(crate) const PYTHON_CALL_TIMEOUT_MS: u64 = 10_000;

pub(crate) struct ModuleGlobals {
    pub(crate) globals: Vec<Value>,
    pub(crate) defined: Vec<bool>,
}

pub struct Vm {
    pub(crate) stack: OperandStack,
    /// The current program's global view: aligned with
    /// `programs[program_id].globals` (functions reference globals by index
    /// into their own program's table).
    pub(crate) globals: Vec<Value>,
    /// Whether each slot in the current view was ever ASSIGNED (a `let` /
    /// `var` declaration or a store). Reading a global that was never
    /// assigned is a ReferenceError in JS — but `typeof` on it is
    /// "undefined" (the TypeOfGlobal opcode skips this check).
    pub(crate) global_defined: Vec<bool>,
    /// Stable name-keyed global table shared across all programs (REPL): a
    /// global set by one line is visible to every later program, even when
    /// their name indices differ.
    pub(crate) global_names: Vec<String>,
    pub(crate) stable_globals: Vec<Value>,
    /// Stable counterpart of `global_defined`, keyed like `stable_globals`.
    pub(crate) stable_defined: Vec<bool>,
    pub(crate) call_stack: Vec<CallFrame>,
    /// Closure cells of the currently executing functions (innermost last).
    pub(crate) cells_stack: Vec<Vec<Rc<RefCell<Value>>>>,
    /// Every program ever loaded (REPL: one per line). Function values carry a
    /// `program` id into this registry, so closures keep executing the bytecode
    /// of the program that defined them even after `set_program` swaps in a
    /// new one.
    pub(crate) programs: Vec<Program>,
    /// Id of the program currently being executed (index into `programs`).
    pub(crate) program_id: u32,
    pub(crate) current_pc: usize,
    /// Bytecode/constants of the currently executing program.
    pub(crate) bytecode: Vec<u8>,
    pub(crate) constants: Vec<Value>,
    /// Saved executions for suspended async invocations and `.then` callbacks.
    /// Promises reference these by id; ids move to `microtasks` when settled.
    pub(crate) continuations: HashMap<u64, Continuation>,
    pub(crate) next_cont_id: u64,
    pub(crate) generators: HashMap<u64, Rc<RefCell<GeneratorState>>>,
    pub(crate) next_gen_id: u64,
    pub(crate) active_generator: Option<u64>,
    /// Settled continuations waiting to run (FIFO). Records live in
    /// `microtask_arena`; the queue holds their addresses. The arena is
    /// bulk-reset (one cursor reset, no per-record free) whenever the queue
    /// drains — the GC-killer pattern.
    pub(crate) microtasks: VecDeque<usize>,
    pub(crate) microtask_arena: ChunkedArena,
    /// Pending `setTimeout` timers.
    pub(crate) timers: Vec<Timer>,
    /// Timestamp used to compute timer deadlines.
    pub(crate) epoch: Instant,
    /// Active exception handlers (innermost last), across all frames. Each
    /// records the frame that owns it so unwinding can scope correctly.
    pub(crate) handlers: Vec<Handler>,
    /// Set when a throw reaches the top level with no handler or async
    /// boundary; read (and cleared) by the CLI/tests via `take_error`.
    pub(crate) uncaught_exception: Option<Value>,
    /// Optional per-`run()` instruction cap (`None` = unlimited, the default
    /// so existing behavior is unchanged). A runaway script (`while(true){}`)
    /// stops the loop and records an uncaught error instead of hanging the
    /// host thread — the JS-side counterpart of the python sidecar's deadline.
    pub(crate) instruction_budget: Option<u64>,
    /// Set by `dispatch` when the budget ran out; consumed (and reset) by
    /// `run` so nested host-initiated dispatches aren't mislabeled uncaught.
    pub(crate) budget_exhausted: bool,
    /// When a native throws into an enclosing try/catch, `throw_value` has
    /// already unwound the stack and pushed the exception at the handler —
    /// this records the handler pc so the dispatcher can jump there instead
    /// of consuming the native's (meaningless) return value.
    pub(crate) native_throw_jump: Option<usize>,
    /// The receiver of the in-flight native call, for `VmHost::this_value`:
    /// the native branch of `dispatch_call` stashes the receiver here before
    /// invoking the native and restores the previous value afterwards, so
    /// re-entrant native calls (a native invoking a JS callback) see their
    /// own receiver. Method-call natives installed on Map/Set prototypes read
    /// their instance from this slot.
    pub(crate) native_this: Option<Value>,
    /// Local extent of the frame-less top-level scope, for the same purpose.
    pub(crate) top_locals_end: usize,
    pub(crate) shared: Arc<SidecarMemory>,
    /// The value heap: every runtime string/array/object allocates here and is
    /// bulk-freed when the VM drops (the GC-killer pillar). Installed as the
    /// active allocation context for the duration of `run()`.
    pub(crate) heap: ArenaHeap,
    /// Second-generation sweep trigger: run the major GC when the old
    /// generation has accumulated more than this many bytes of churn since
    /// the last sweep (churn = replaced globals/closures, whether they bump
    /// or reuse free space). Adaptive: backed off when a major reclaims
    /// little, tightened when it reclaims a lot.
    pub(crate) major_threshold: usize,
    /// Cumulative old-generation allocations at the last major GC; the delta
    /// from here is the churn that triggers the next one.
    pub(crate) last_major_alloc: usize,
    /// Incremental major-GC mark in progress. `Some` while a second-generation
    /// sweep is being prepared over multiple unit boundaries; each boundary
    /// records newly-reached boxes, traces `mark_budget` queued ones, and
    /// re-traces barrier-dirtied Rc structures — so no single request pays
    /// for walking the whole live graph.
    pub(crate) mark: Option<MarkState>,
    /// Worklist boxes traced per unit boundary while a mark is in progress
    /// (bounds the per-request mark stall).
    pub(crate) mark_budget: usize,

    /// Compiled regex programs, keyed by (pattern, flags). The compiled
    /// program is immutable and shared (Arc) across every literal evaluation;
    /// each MakeRegex builds a fresh per-object state (own lastIndex) from
    /// it. Compiling happens once per distinct literal, and the lexer already
    /// validated the pattern, so the runtime path is a cache hit.
    pub(crate) regex_cache: HashMap<(String, String), Arc<RegexCompiled>>,

    /// Cached Python module objects (one per imported `.py` file): the
    /// natives inside call into the matching sidecar below. Walked as GC
    /// roots so the object boxes survive arena sweeps.
    pub(crate) python_modules: HashMap<String, Value>,
    /// Dedicated worker threads for imported `.py` files (one per file), each
    /// servicing a request queue. Declared after `shared` so teardown order is
    /// explicit in `Drop` (children die and are joined before the segment's
    /// backing file is deleted).
    pub(crate) python_workers: HashMap<String, PythonWorker>,
    /// Per-call python timeout override for pools this VM spawns
    /// (`set_python_timeout`). `None` = read `ALLOY_PYTHON_TIMEOUT_MS` at
    /// pool growth, defaulting to `PYTHON_CALL_TIMEOUT_MS`.
    pub(crate) python_timeout: Option<std::time::Duration>,
    /// In-flight async python calls, by call id: the promise each will
    /// settle plus the src/gen/line needed to re-run a call that a reload
    /// aborted mid-flight. Completions arrive on `python_rx`; the VM thread
    /// resolves them (promises must settle on the VM thread — the values
    /// they carry allocate into the thread-local arena heap).
    pub(crate) python_inflight_calls: HashMap<u64, InflightPyCall>,
    /// Number of python calls still awaiting a completion (drives the event
    /// loop's poll cadence while they are in flight).
    pub(crate) python_inflight: usize,
    /// `.py` paths this VM has ever started a worker pool for (first import
    /// vs rebuild bookkeeping for `python_rebuilds`).
    pub(crate) python_started: std::collections::HashSet<String>,
    /// Number of times a `.py` worker pool was torn down and rebuilt after
    /// its first import (reload staleness). Asserted by the burst-coalescing
    /// test: a burst of reloads must rebuild exactly once, not once per
    /// reload. Read-only after run; not a GC root (plain strings).
    pub(crate) python_rebuilds: usize,
    /// Worker threads send `(src, child_idx, call_id, raw response line)`
    /// here; the VM drains it at event-loop boundaries, frees the child's
    /// in-flight slot, and resolves the matching promise.
    pub(crate) python_tx: mpsc::Sender<(String, usize, u64, String)>,
    pub(crate) python_rx: mpsc::Receiver<(String, usize, u64, String)>,
    /// Cross-thread `spawn(fn)`: the worker runs the function in an isolated
    /// VM and sends its serialized result here; the VM thread drains at
    /// event-loop boundaries and resolves the matching promise (values must
    /// settle on the VM thread — they allocate into its arena heap).
    pub(crate) spawn_tx: mpsc::Sender<(u64, Vec<u8>)>,
    pub(crate) spawn_rx: mpsc::Receiver<(u64, Vec<u8>)>,
    /// In-flight spawned tasks: the promise each will settle, by task id.
    pub(crate) spawn_inflight: HashMap<u64, Value>,
    /// Number of spawned tasks still awaiting a completion (keeps the event
    /// loop pumping while a worker runs).
    pub(crate) spawn_pending: usize,
    pub(crate) next_spawn_id: u64,
    /// Worker threads, joined at teardown so no task outlives the VM.
    pub(crate) spawn_workers: Vec<std::thread::JoinHandle<()>>,

    /// Loaded module programs: pid → its own global scope. Modules run in an
    /// isolated namespace (their `let`/`const`/`function` declarations never
    /// leak into the requirer, and vice versa) and keep their final globals
    /// here so functions the module exported can be called later — each call
    /// swaps this view in via `load_program`.
    pub(crate) modules: HashMap<u32, ModuleGlobals>,
    /// `require('./x.ajs')` → (exports object, shared generation cell, the
    /// generation it was loaded at), so each file runs exactly once per
    /// generation on this thread (module singletons, like Node). The cell is
    /// shared with every other VM, so a `reload()` on any thread invalidates
    /// this entry at the next require. Also walked as a GC root.
    pub(crate) require_cache: HashMap<String, (Value, Arc<ModuleGen>, u64)>,
    /// Stack of module paths currently being loaded, for circular-require
    /// detection (a loud error instead of Node's partial-module surprise).
    pub(crate) requiring: Vec<String>,
    /// Directory of the file currently executing (set by the CLI for the
    /// main script, pushed/popped around each required module). Relative
    /// `require('./x.ajs')` paths resolve against this, like Node; None
    /// means the process working directory (REPL).
    pub(crate) current_dir: Option<std::path::PathBuf>,
    /// Process-wide compiled-module cache shared with every spawn worker:
    /// compile once per module generation, load in any VM, and reload across
    /// threads without racing.
    pub(crate) registry: Arc<SharedModuleRegistry>,
    /// Cross-thread wake pipe: another VM's `send` routes a settlement to
    /// this VM's inbox (in `wake_tx`) and pushes a token here; the event
    /// loop's `recv_timeout` then returns immediately instead of waiting out
    /// its poll cadence. The handle (`wake_tx`) is stamped onto every
    /// promise this VM creates so settlements are routed back to the owner.
    pub(crate) wake_rx: std::sync::mpsc::Receiver<()>,
    pub(crate) wake_tx: Arc<WakeHandle>,
    /// Process-wide `.py` sidecar reload tracking shared with every spawn
    /// worker: a reload that starts a new burst on any thread re-imports the
    /// file on every VM's next call; reloads folded into the current burst
    /// don't (burst coalescing).
    pub(crate) py_registry: Arc<SharedPyRegistry>,
    /// Promises this VM has parked that are exposed to other threads (a
    /// channel `recv` waiter): their settlements can arrive via `wake_tx`,
    /// so the event loop keeps pumping until they settle. Also a GC root.
    pub(crate) cross_waiters: Vec<Value>,
    /// When set, `print` writes here instead of stdout (used by tests).
    pub(crate) output_sink: Option<Arc<Mutex<Vec<String>>>>,
    /// The seven error constructors seeded as one group (subclass prototypes
    /// chain to the base Error.prototype), built once per VM.
    pub(crate) error_seeds: Option<hashbrown::HashMap<String, Value>>,
    /// Direct-mapped 2-way polymorphic inline cache for GetProperty/SetProperty.
    pub(crate) ic: Box<[IcPoly; IC_SLOTS]>,
    /// Call-site cache for `Call`/`CallMethod` (direct-mapped like `ic`).
    pub(crate) call_ic: Box<[CallIcEntry; IC_SLOTS]>,
    /// Cached `ALLOY_OP_HIST` flag (checked once at construction, not per-instr).
    pub(crate) op_hist_on: bool,
    /// Backwards-jump trip counts for the baseline-JIT hypervisor: `pc -> trips`.
    /// Incremented only on taken backwards jumps (loop back-edges). When a count
    /// crosses `HOT_THRESHOLD`, a line is logged with `ALLOY_JIT_LOG=1`.
    pub(crate) backedge_counts: hashbrown::HashMap<usize, u32>,
    pub(crate) jit: Option<Box<crate::jit::JitEngine>>,
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
    pub(crate) fn new_worker(
        program: Program,
        registry: Arc<SharedModuleRegistry>,
        py_registry: Arc<SharedPyRegistry>,
        current_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self::new_inner(program, None, registry, py_registry, current_dir)
    }

    pub(crate) fn new_inner(
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
            current_pc: 0,
            bytecode,
            constants,
            continuations: HashMap::new(),
            next_cont_id: 0,
            generators: HashMap::new(),
            next_gen_id: 0,
            active_generator: None,
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
            jit: crate::jit::JitEngine::new().ok().map(Box::new),
        }
    }

    /// Track that a local slot is live, so exception unwinding preserves it.
    pub(crate) fn record_local(&mut self, idx: usize) {
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
    pub(crate) fn load_program(&mut self, pid: u32) {
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
    pub(crate) fn seed_global_named(&mut self, name: &str) -> Value {
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

    pub fn get_global(&self, name: &str) -> Option<Value> {
        self.globals
            .iter()
            .zip(self.global_names.iter())
            .find(|(_, n)| n.as_str() == name)
            .map(|(v, _)| v.clone())
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, val: Value) {
        self.stack.push(val);
    }

    #[inline(always)]
    pub(crate) fn pop(&mut self) -> Value {
        self.stack.pop()
    }

    #[inline(always)]
    pub(crate) fn peek(&self) -> Value {
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

    pub(crate) fn get_fn_name(&self, val: &Value) -> Option<String> {
        if let Some(fd) = val.as_function() {
            if let Some(prog) = self.programs.get(fd.program as usize) {
                if let Some(name) = prog.function_name_at(fd.ptr) {
                    return Some(name.to_string());
                }
            }
        }
        if let Some(obj) = val.as_object() {
            let od = obj.borrow();
            if let Some(name_val) = od.get("name") {
                if let Some(s) = name_val.as_str() {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
        None
    }

    pub(crate) fn get_frame_location(&self, program_id: u32, pc: usize) -> (String, u32, u32) {
        if let Some(prog) = self.programs.get(program_id as usize) {
            let file = prog.source_file.clone().unwrap_or_else(|| "<anonymous>".to_string());
            if let Some((line, col)) = prog.get_location(pc) {
                (file, line, col)
            } else {
                (file, 1, 1)
            }
        } else {
            ("<anonymous>".to_string(), 1, 1)
        }
    }

}

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

    fn format_stack_trace(&self, name: &str, msg: &str) -> String {
        let mut out = if msg.is_empty() {
            name.to_string()
        } else {
            format!("{}: {}", name, msg)
        };

        // Top frame: currently executing function
        let top_fn = self.call_stack.last()
            .and_then(|cf| self.get_fn_name(&cf.fn_value))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let (file, line, col) = self.get_frame_location(self.program_id, self.current_pc);
        out.push_str(&format!("\n    at {} ({}:{}:{})", top_fn, file, line, col));

        // Caller frames
        for (i, cf) in self.call_stack.iter().rev().enumerate() {
            let caller_fn = if i + 1 < self.call_stack.len() {
                let prev_frame = &self.call_stack[self.call_stack.len() - 2 - i];
                self.get_fn_name(&prev_frame.fn_value).unwrap_or_else(|| "<anonymous>".to_string())
            } else {
                "<anonymous>".to_string()
            };
            let (file, line, col) = self.get_frame_location(cf.return_program, cf.return_addr);
            out.push_str(&format!("\n    at {} ({}:{}:{})", caller_fn, file, line, col));
        }

        out
    }

    fn capture_stack_trace(&mut self, target: &Value, constructor_opt: Option<&Value>) {
        let (name, msg) = if let Some(od) = target.as_object() {
            let od = od.borrow();
            let n = od.get("name").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_else(|| "Error".to_string());
            let m = od.get("message").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_default();
            (n, m)
        } else {
            ("Error".to_string(), String::new())
        };

        let mut frames: Vec<(String, String, u32, u32, Option<Value>)> = Vec::new();
        // Top frame
        let top_fn = self.call_stack.last()
            .and_then(|cf| self.get_fn_name(&cf.fn_value))
            .unwrap_or_else(|| "<anonymous>".to_string());
        let (file, line, col) = self.get_frame_location(self.program_id, self.current_pc);
        let top_val = self.call_stack.last().map(|cf| cf.fn_value.clone());
        frames.push((top_fn, file, line, col, top_val));

        // Caller frames
        for (i, cf) in self.call_stack.iter().rev().enumerate() {
            let (caller_fn, caller_val) = if i + 1 < self.call_stack.len() {
                let prev_frame = &self.call_stack[self.call_stack.len() - 2 - i];
                (
                    self.get_fn_name(&prev_frame.fn_value).unwrap_or_else(|| "<anonymous>".to_string()),
                    Some(prev_frame.fn_value.clone()),
                )
            } else {
                ("<anonymous>".to_string(), None)
            };
            let (file, line, col) = self.get_frame_location(cf.return_program, cf.return_addr);
            frames.push((caller_fn, file, line, col, caller_val));
        }

        // If constructor_opt is provided, omit all frames up to and including the match
        if let Some(ctor) = constructor_opt {
            if let Some(pos) = frames.iter().position(|(_, _, _, _, v)| {
                if let Some(fv) = v {
                    crate::vm::alu::strict_equal(fv, ctor)
                } else {
                    false
                }
            }) {
                frames.drain(0..=pos);
            }
        }

        let mut out = if msg.is_empty() {
            name
        } else {
            format!("{}: {}", name, msg)
        };

        for (fn_name, file, line, col, _) in frames {
            out.push_str(&format!("\n    at {} ({}:{}:{})", fn_name, file, line, col));
        }

        if let Some(od) = target.as_object() {
            od.borrow_mut().set("stack", Value::string(out));
        }
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
                generator_id: None,
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
                generator_id: None,
            });
            self.dispatch(fn_ptr as usize)
        } else {
            Value::undefined()
        }
    }

    fn generator_step(&mut self, gen_id: u64, val: Value, is_throw: bool) -> Value {
        self.generator_step(gen_id, val, is_throw)
    }

    fn generator_return(&mut self, gen_id: u64, val: Value) -> Value {
        self.generator_return(gen_id, val)
    }
}
