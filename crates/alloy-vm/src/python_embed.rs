//! In-process CPython embedding — the PRD's "bound CPython interpreter".
//!
//! Opt-in at runtime with `ALLOY_PYTHON_EMBED=1`. When set, `.py` calls run
//! in THIS process against a lazily-initialized CPython interpreter loaded
//! from the installed python (`python3.dll` on Windows / `libpython3.so` on
//! Unix, discovered next to `ALLOY_PYTHON` or `python`/`python3`), instead
//! of a subprocess child. The library is loaded at **runtime** via
//! `LoadLibrary`/`dlopen` — never linked — so a build stays portable and a
//! binary without Python on PATH still starts; embed mode simply reports
//! itself unavailable and falls back to the child sidecar.
//!
//! The data plane is the shared segment, accessed by raw pointer: the user
//! module's `buf` is a `ctypes` array mapped directly over the segment's
//! bytes (`from_address`), so a JS buffer's data is read/written in place —
//! no serialization, no second mmap, no pipe. Function *calls* reuse the
//! exact same line-based wire format as the child sidecar (`build_line` +
//! `decode_wire` on the VM side), so the async pool, reload bursts, and
//! promise settling in the VM are unchanged.
//!
//! Honest tradeoffs vs the child sidecar:
//! - The GIL serializes in-process calls, so the worker pool is capped at
//!   one backend per file (same-file calls queue; different files also
//!   contend on the GIL).
//! - The per-call timeout is **cooperative** (`ALLOY_PYTHON_TIMEOUT_MS`,
//!   default 10s): a watchdog raises `KeyboardInterrupt` in the executing
//!   worker thread via `PyThreadState_SetAsyncExc` (targeting the worker's
//!   published OS thread id), firing at the next bytecode boundary. Pure-python loops are interrupted; a function blocked in a
//!   C call with the GIL released (`time.sleep`, blocking I/O) is not
//!   interrupted until it returns to python — unlike the child mode's
//!   kill-the-process timeout, which handles even those.
//! - The interpreter is process-global: one init per process, shared by all
//!   `.py` files (module dicts are per-file backends, released on drop).
//! Gains: no process spawn per file, no pipe round-trip, reload is an
//! in-place re-exec, and the segment is reached by direct pointer.
//!
//! **Teardown:** library hosts may shut the interpreter down cleanly with
//! [`finalize_interpreter`] once every VM that imported `.py` files has been
//! dropped (all embed backends released — [`live_backends`] reports the
//! count). Finalization is terminal and guarded: it refuses while any
//! backend is alive, and after it succeeds embed mode is disabled for the
//! rest of the process (further `.py` imports fall back to the child
//! sidecar), so a host that finalizes early degrades gracefully instead of
//! crashing later.

use std::ffi::{c_char, c_int, c_ulong, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

type PyObj = *mut std::ffi::c_void;
type PyGILState = c_int;

/// Every symbol the embed path needs, resolved once from the loaded python
/// library. All are stable-ABI entry points.
#[derive(Clone, Copy)]
struct PyApi {
    py_initialize_ex: unsafe extern "C" fn(c_int),
    py_eval_save_thread: unsafe extern "C" fn() -> *mut std::ffi::c_void,
    pygilstate_ensure: unsafe extern "C" fn() -> PyGILState,
    pygilstate_release: unsafe extern "C" fn(PyGILState),
    py_dec_ref: unsafe extern "C" fn(PyObj),
    py_long_from_longlong: unsafe extern "C" fn(i64) -> PyObj,
    py_float_from_double: unsafe extern "C" fn(f64) -> PyObj,
    py_unicode_from_string: unsafe extern "C" fn(*const c_char) -> PyObj,
    /// Stable-ABI type check (`PyObject_IsInstance`) — the type-check macros
    /// (`PyLong_Check` & co.) are header-only and not exported.
    py_object_isinstance: unsafe extern "C" fn(PyObj, PyObj) -> c_int,
    py_long_as_longlong: unsafe extern "C" fn(PyObj) -> i64,
    py_unicode_as_utf8_and_size: unsafe extern "C" fn(PyObj, *mut isize) -> *const c_char,
    py_object_str: unsafe extern "C" fn(PyObj) -> PyObj,
    py_object_call: unsafe extern "C" fn(PyObj, PyObj, PyObj) -> PyObj,
    py_object_getattr_string: unsafe extern "C" fn(PyObj, *const c_char) -> PyObj,
    py_tuple_new: unsafe extern "C" fn(isize) -> PyObj,
    py_tuple_set_item: unsafe extern "C" fn(PyObj, isize, PyObj) -> c_int,
    py_list_size: unsafe extern "C" fn(PyObj) -> isize,
    py_list_get_item: unsafe extern "C" fn(PyObj, isize) -> PyObj,
    py_tuple_size: unsafe extern "C" fn(PyObj) -> isize,
    py_tuple_get_item: unsafe extern "C" fn(PyObj, isize) -> PyObj,
    py_dict_set_item_string: unsafe extern "C" fn(PyObj, *const c_char, PyObj) -> c_int,
    py_import_importmodule: unsafe extern "C" fn(*const c_char) -> PyObj,
    py_module_new: unsafe extern "C" fn(*const c_char) -> PyObj,
    py_module_get_dict: unsafe extern "C" fn(PyObj) -> PyObj,
    py_run_string: unsafe extern "C" fn(*const c_char, c_int, PyObj, PyObj) -> PyObj,
    py_err_fetch: unsafe extern "C" fn(*mut PyObj, *mut PyObj, *mut PyObj),
    py_err_clear: unsafe extern "C" fn(),
    /// Raise `exc` asynchronously in the thread with the given id at its
    /// next bytecode boundary — the documented cross-thread interruption
    /// API. Note: the first parameter is the THREAD ID (`unsigned long`),
    /// not a `PyThreadState*`; the lookup returns 0 when the thread is gone.
    py_thread_state_set_async_exc: unsafe extern "C" fn(c_ulong, PyObj) -> c_int,
    /// The calling thread's OS thread id — what the worker records in its
    /// arm so the watchdog can target it (the GILState-created
    /// `PyThreadState` carries exactly this id, and is found by
    /// `PyThreadState_SetAsyncExc`).
    py_thread_get_thread_ident: unsafe extern "C" fn() -> c_ulong,
    py_finalize_ex: unsafe extern "C" fn() -> c_int,
}

/// The loaded library handle (kept alive for the process) + resolved API +
/// cached builtins type objects (singletons; kept for the process).
struct PyRuntime {
    api: PyApi,
    #[allow(dead_code)]
    handle: Handle,
    types: PyTypes,
}

/// Borrowed-type cache: the builtins type objects used for stable-ABI
/// `PyObject_IsInstance` checks. These are process-lifetime singletons; the
/// refs are never released (the interpreter is never finalized).
#[derive(Clone, Copy)]
struct PyTypes {
    bool_ty: PyObj,
    int_ty: PyObj,
    float_ty: PyObj,
    str_ty: PyObj,
    list_ty: PyObj,
    tuple_ty: PyObj,
    /// The `None` singleton, fetched from `builtins.None` (a data-symbol
    /// `GetProcAddress("Py_None")` is not reliably exported).
    none_ty: PyObj,
    /// `builtins.KeyboardInterrupt` — what the cooperative per-call timeout
    /// raises in the executing thread (CPython's own signal machinery does
    /// exactly this via `Py_AddPendingCall`).
    keyboard_interrupt: PyObj,
}

// Safety: the api is a bundle of plain function pointers and the handle is
// an opaque library handle; all use of the interpreter is serialized by the
// GIL. Nothing here is mutated after init.
unsafe impl Send for PyRuntime {}
unsafe impl Sync for PyRuntime {}

/// Process-global interpreter: initialized exactly once per process. An
/// `Err` (embed disabled / library missing) is cached, so every later start
/// falls back to the child sidecar without retrying.
static PY_RUNTIME: OnceLock<Result<Arc<PyRuntime>, String>> = OnceLock::new();

/// Live embed backends (imported `.py` files that still hold a module
/// reference). [`finalize_interpreter`] refuses to run while this is non-zero
/// — finalizing with live module pointers would leave dangling references in
/// the interpreter.
static LIVE_BACKENDS: AtomicUsize = AtomicUsize::new(0);

/// Set once [`finalize_interpreter`] succeeds (or attempted and failed at
/// the interpreter level): after that, embed mode is off for the process and
/// new `.py` imports fall back to the child sidecar.
static FINALIZED: AtomicBool = AtomicBool::new(false);

/// The raw C machinery behind the cooperative per-call timeout: the
/// `PyThreadState_SetAsyncExc` function pointer, the `KeyboardInterrupt`
/// exception object, and the GILState pair the watchdog needs to call
/// `SetAsyncExc` SAFELY — CPython requires it to be called with the GIL
/// held (it dereferences the global current thread state, and would
/// access-violate if no thread is current). The interrupt is delivered by
/// the watchdog to the worker's thread id — NOT via `Py_AddPendingCall`,
/// which empirically only fires in the main thread.
struct InterruptState {
    set_async_exc: unsafe extern "C" fn(c_ulong, PyObj) -> c_int,
    exc: PyObj,
    gilstate_ensure: unsafe extern "C" fn() -> c_int,
    gilstate_release: unsafe extern "C" fn(c_int),
}

// Safety: the exception object is a builtins singleton and the function
// pointer is a plain code address; `PyThreadState_SetAsyncExc` is
// documented as safe to call from any thread without the GIL.
unsafe impl Send for InterruptState {}
unsafe impl Sync for InterruptState {}

static INTERRUPT_STATE: OnceLock<InterruptState> = OnceLock::new();

/// Gens whose deadline fired. The watchdog records the gen (under
/// [`TEARDOWN_LOCK`], right before raising) so a worker's fetch — which can
/// only run after the interrupt was delivered — sees its OWN gen here and
/// reports the cooperative timeout. Per-gen, not a single flag: with many
/// VMs sharing one interpreter, a process-global bool could be reset by an
/// unrelated call between the fire and the fetch, misclassifying our
/// interrupt as a user `raise KeyboardInterrupt`. Entries are removed when
/// the owning call stands down (fetch or cancel), so the set is nearly
/// always empty.
static FIRED_GENS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Consume (remove) `gen` from the fired set, returning whether a fire for
/// it was recorded. Idempotent — safe to call twice (fetch + cancel).
fn fired_gen(gen: usize) -> bool {
    match FIRED_GENS.lock() {
        Ok(mut g) => {
            let hit = g.contains(&gen);
            g.retain(|&x| x != gen);
            hit
        }
        Err(p) => p.into_inner().contains(&gen),
    }
}

/// Serializes the watchdog's `PyThreadState_SetAsyncExc` (which walks the
/// interpreter's thread-state list) against every `PyGILState_Release`
/// (which deletes that thread's GILState tstate) in the embed module — not
/// just `call_line`, but `start`/`restart`/`shutdown` too. Without this
/// lock, a fire could walk the list while any of those tears a tstate down
/// — a use-after-free that crashes under parallel load. With it, a
/// non-zero tid loaded under the lock always belongs to a live tstate.
/// The lock is never held while waiting for the GIL, so there is no
/// ordering cycle with the interpreter's own lock.
static TEARDOWN_LOCK: Mutex<()> = Mutex::new(());

/// Per-call token, bumped on every call: lets `Cancel { gen }` remove
/// exactly the arm it belongs to (unrelated calls never disarm each other).
static GEN_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Commands for the process-global watchdog thread.
enum WatchdogCmd {
    /// Arm the deadline for the call identified by (tid, gen).
    Arm {
        tid: usize,
        gen: usize,
        deadline: Instant,
    },
    /// Stand down the call identified by `gen` (it finished first). Token-
    /// scoped: an unrelated call's cancel can never disarm THIS call's
    /// deadline (a real hazard with a single shared deadline slot when two
    /// VMs' calls interleave on the channel).
    Cancel { gen: usize },
}

/// The watchdog: ONE thread per process, holding a small set of armed
/// deadlines (calls are GIL-serialized, so the set is nearly always empty
/// or single-entry; multiple entries only transiently when a call finishes
/// while another armed one is in flight). It sleeps until the nearest
/// deadline, then requests the interrupt via `PyThreadState_SetAsyncExc`
/// (documented as callable from any thread without the GIL), targeting the
/// EXACT thread id each arm recorded. The arm itself is the liveness
/// record: a call that completes sends `Cancel { gen }`, removing its
/// entry; a fire therefore only happens for a call that never canceled.
/// (Residual race: if a call finishes and its worker starts a NEW call on
/// the same thread while the cancel is still in flight, the interrupt can
/// land in that next call — a spurious, recoverable timeout rejection.)
/// Fires happen under [`TEARDOWN_LOCK`] so the thread-state walk never
/// races a teardown.
static WATCHDOG_TX: OnceLock<Mutex<mpsc::Sender<WatchdogCmd>>> = OnceLock::new();

fn watchdog_ensure() -> &'static Mutex<mpsc::Sender<WatchdogCmd>> {
    WATCHDOG_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<WatchdogCmd>();
        std::thread::Builder::new()
            .name("alloy-embed-watchdog".to_string())
            .spawn(move || {
                let mut armed: Vec<(usize, usize, Instant)> = Vec::new();
                loop {
                    // 1. Drain pending commands BEFORE firing: a Cancel
                    //    already in the channel stands the arm down before
                    //    the fire pass runs, so a completed call's cancel
                    //    always beats a just-expired deadline (closing the
                    //    spurious-interrupt-into-the-next-call race for
                    //    commands already sent).
                    loop {
                        match rx.try_recv() {
                            Ok(WatchdogCmd::Arm {
                                tid,
                                gen,
                                deadline: at,
                            }) => armed.push((tid, gen, at)),
                            Ok(WatchdogCmd::Cancel { gen }) => armed.retain(|&(_, g, _)| g != gen),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }
                    // 2. Fire every expired deadline: raise in the exact
                    //    thread the arm recorded. The fired gen is recorded
                    //    BEFORE the interrupt lands (under the same lock),
                    //    so the worker's fetch — which can only happen
                    //    after the interrupt was delivered — sees its own
                    //    gen in the fired set and reports the cooperative
                    //    timeout. `PyThreadState_SetAsyncExc` must run with
                    //    the GIL held (it derefs the global current
                    //    tstate), so the watchdog takes it via GILState
                    //    first; a python holder cooperatively drops it
                    //    within a switch interval. The release runs under
                    //    the teardown lock, matching the worker's Gil
                    //    ordering.
                    let now = Instant::now();
                    let mut i = 0;
                    while i < armed.len() {
                        if armed[i].2 <= now {
                            let (armed_tid, armed_gen, _) = armed.remove(i);
                            if armed_tid != 0 {
                                if let Some(st) = INTERRUPT_STATE.get() {
                                    let gstate = unsafe { (st.gilstate_ensure)() };
                                    let _lock =
                                        TEARDOWN_LOCK.lock().unwrap_or_else(|g| g.into_inner());
                                    if let Ok(mut fired) = FIRED_GENS.lock() {
                                        fired.push(armed_gen);
                                    }
                                    unsafe {
                                        (st.set_async_exc)(armed_tid as c_ulong, st.exc);
                                    }
                                    drop(_lock);
                                    unsafe { (st.gilstate_release)(gstate) };
                                }
                            }
                        } else {
                            i += 1;
                        }
                    }
                    // 3. Wait for a command, or until the nearest deadline.
                    let wait = armed
                        .iter()
                        .map(|(_, _, d)| d.saturating_duration_since(Instant::now()))
                        .min()
                        .unwrap_or(Duration::from_secs(3600));
                    match rx.recv_timeout(wait) {
                        Ok(WatchdogCmd::Arm {
                            tid,
                            gen,
                            deadline: at,
                        }) => armed.push((tid, gen, at)),
                        Ok(WatchdogCmd::Cancel { gen }) => armed.retain(|&(_, g, _)| g != gen),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn embed watchdog thread");
        Mutex::new(tx)
    })
}

fn watchdog_arm(tid: usize, gen: usize, deadline: Instant) {
    let _ = watchdog_ensure()
        .lock()
        .unwrap_or_else(|g| g.into_inner())
        .send(WatchdogCmd::Arm { tid, gen, deadline });
}

fn watchdog_cancel(gen: usize) {
    if let Some(tx) = WATCHDOG_TX.get() {
        let _ = tx
            .lock()
            .unwrap_or_else(|g| g.into_inner())
            .send(WatchdogCmd::Cancel { gen });
    }
}

#[cfg(windows)]
type Handle = windows_sys::Win32::Foundation::HMODULE;
#[cfg(unix)]
type Handle = *mut std::ffi::c_void;

/// True when in-process embedding is active: `ALLOY_PYTHON_EMBED=1` AND the
/// interpreter loaded successfully (and has not been finalized). The
/// failure (if any) is logged once.
pub fn embed_enabled() -> bool {
    if FINALIZED.load(Ordering::Relaxed) {
        return false;
    }
    PY_RUNTIME.get_or_init(init_runtime).is_ok()
}

/// The interpreter, or the reason it is unavailable.
fn runtime() -> Result<&'static Arc<PyRuntime>, String> {
    if FINALIZED.load(Ordering::Relaxed) {
        return Err("the embed python interpreter was finalized".into());
    }
    PY_RUNTIME
        .get_or_init(init_runtime)
        .as_ref()
        .map_err(|e| e.clone())
}

/// How many embed backends (imported `.py` files) are still alive. A host
/// should drop every VM that touched python (tearing down its pools and
/// releasing backends) until this reaches zero before calling
/// [`finalize_interpreter`].
pub fn live_backends() -> usize {
    LIVE_BACKENDS.load(Ordering::Relaxed)
}
/// True once [`finalize_interpreter`] ran (successfully or not at the
/// interpreter level) — embed mode is off for the rest of the process.
pub fn is_finalized() -> bool {
    FINALIZED.load(Ordering::Relaxed)
}

/// Cleanly shut the embedded interpreter down (`Py_FinalizeEx`). Optional:
/// for library-embedded hosts that want python fully torn down at shutdown
/// (flushed std streams, released resources) instead of relying on process
/// exit. **Safety contract:** every embed backend must be gone first — the
/// caller drops all VMs that imported `.py` files (pool teardown joins the
/// worker threads and releases the module references). This is enforced:
/// with any backend still alive the call refuses, so a host can never
/// finalize over a live module pointer.
///
/// Finalization is terminal: after it succeeds (or fails at the interpreter
/// level), embed mode is disabled for the rest of the process and later
/// `.py` imports use the child sidecar. Returns the `Py_FinalizeEx` result
/// (an error means flushing std streams failed).
pub fn finalize_interpreter() -> Result<(), String> {
    let alive = LIVE_BACKENDS.load(Ordering::Relaxed);
    if alive > 0 {
        return Err(format!(
            "{} embed python module(s) still alive — drop their VMs (or shutdown_python_workers) before finalizing",
            alive
        ));
    }
    let rt = runtime()?;
    let code = unsafe {
        // The initializing thread released the GIL at init; re-acquire it
        // for finalization. Py_FinalizeEx destroys the thread state, so the
        // GIL must NOT be released afterwards.
        (rt.api.pygilstate_ensure)();
        (rt.api.py_finalize_ex)()
    };
    FINALIZED.store(true, Ordering::Relaxed);
    if code == 0 {
        Ok(())
    } else {
        Err("Py_FinalizeEx reported an error while flushing std streams".into())
    }
}

fn init_runtime() -> Result<Arc<PyRuntime>, String> {
    let embed_requested = std::env::var("ALLOY_PYTHON_EMBED").is_ok();
    let init = || -> Result<Arc<PyRuntime>, String> {
        if std::env::var("ALLOY_PYTHON_EMBED").as_deref() != Ok("1") {
            return Err("ALLOY_PYTHON_EMBED is not set to 1".into());
        }
        let (api, handle) = load_api()?;
        let types = unsafe {
            // The initializing thread holds the GIL here (acquired by
            // Py_InitializeEx); load the builtins types, then release the
            // GIL so worker threads can acquire it per-call.
            (api.py_initialize_ex)(0);
            let types = load_builtin_types(&api)?;
            (api.py_eval_save_thread)();
            types
        };
        Ok(Arc::new(PyRuntime { api, handle, types }))
    };
    let r = init();
    // Only speak up when the operator opted in and it failed — plain child
    // mode (the default) stays silent.
    if let Err(e) = &r {
        if embed_requested {
            eprintln!(
                "[alloy] python embed unavailable ({}); using child sidecars",
                e
            );
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Dynamic loading (never a link-time dependency).
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn candidate_libraries() -> Vec<Vec<u8>> {
    // Only bare names here — the loader-path search (a PATH-loaded forwarder
    // reveals the full DLL's directory via `loaded_library_dir`, and the
    // `python_home_dir` probe is the last resort). Never spawn python
    // eagerly: embed cold start must stay subprocess-free.
    vec![b"python3.dll\0".to_vec()]
}

#[cfg(unix)]
fn candidate_libraries() -> Vec<Vec<u8>> {
    vec![
        b"libpython3.so\0".to_vec(),
        b"libpython3.12.so\0".to_vec(),
        b"libpython3.11.so\0".to_vec(),
        b"libpython3.10.so\0".to_vec(),
    ]
}

/// The python executable's directory (`ALLOY_PYTHON` or the PATH default),
/// used to find the DLL when it is not on the loader path.
fn python_home_dir() -> Option<String> {
    let py = std::env::var("ALLOY_PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    });
    let out = std::process::Command::new(&py)
        .arg("-c")
        .arg("import sys,os;print(os.path.dirname(sys.executable))")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(windows)]
fn open_library_candidate(cand: &[u8]) -> Result<Handle, String> {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;
    let h = unsafe { LoadLibraryA(cand.as_ptr()) };
    if h.is_null() {
        Err(format!(
            "cannot load {}",
            String::from_utf8_lossy(&cand[..cand.len() - 1])
        ))
    } else {
        Ok(h)
    }
}

#[cfg(windows)]
fn close_library(h: Handle) {
    use windows_sys::Win32::Foundation::FreeLibrary;
    // The library also gets unloaded at process exit; closing a failed
    // candidate just avoids pinning it.
    unsafe { FreeLibrary(h) };
}

#[cfg(unix)]
fn open_library_candidate(cand: &[u8]) -> Result<Handle, String> {
    let c = CString::new(&cand[..cand.len() - 1]).map_err(|_| "bad library name".to_string())?;
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        Err(format!("cannot load {}", c.to_string_lossy()))
    } else {
        Ok(h)
    }
}

#[cfg(unix)]
fn close_library(h: Handle) {
    unsafe { libc::dlclose(h) };
}

#[cfg(windows)]
unsafe fn get_sym(h: Handle, name: &CStr) -> Result<*mut std::ffi::c_void, String> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    match GetProcAddress(h, name.as_ptr() as *const u8) {
        Some(p) => Ok(p as *mut std::ffi::c_void),
        None => Err(format!(
            "python library is missing symbol {}",
            name.to_string_lossy()
        )),
    }
}

#[cfg(unix)]
unsafe fn get_sym(h: Handle, name: &CStr) -> Result<*mut std::ffi::c_void, String> {
    let p = libc::dlsym(h, name.as_ptr());
    if p.is_null() {
        Err(format!(
            "python library is missing symbol {}",
            name.to_string_lossy()
        ))
    } else {
        Ok(p)
    }
}

/// Resolve `name` and transmute the raw address to a function pointer.
unsafe fn load_fn<T: Copy>(h: Handle, name: &CStr) -> Result<T, String> {
    let sym = get_sym(h, name)?;
    Ok(std::mem::transmute_copy::<*mut std::ffi::c_void, T>(&sym))
}

macro_rules! c_string {
    ($s:literal) => {
        CStr::from_bytes_with_nul_unchecked(concat!($s, "\0").as_bytes())
    };
}

/// Load the first candidate library in which EVERY required symbol resolves.
/// The stable-ABI forwarder (`python3.dll`) lacks some exports (type-check
/// macros aside, `PyRun_String` is not in the limited API), so a candidate
/// that loads but misses symbols is closed and the next is tried — the full
/// `python3XX.dll` always wins.
unsafe fn try_load_api(h: Handle) -> Result<PyApi, String> {
    Ok(PyApi {
        py_initialize_ex: load_fn(h, c_string!("Py_InitializeEx"))?,
        py_eval_save_thread: load_fn(h, c_string!("PyEval_SaveThread"))?,
        pygilstate_ensure: load_fn(h, c_string!("PyGILState_Ensure"))?,
        pygilstate_release: load_fn(h, c_string!("PyGILState_Release"))?,
        py_dec_ref: load_fn(h, c_string!("Py_DecRef"))?,
        py_long_from_longlong: load_fn(h, c_string!("PyLong_FromLongLong"))?,
        py_float_from_double: load_fn(h, c_string!("PyFloat_FromDouble"))?,
        py_unicode_from_string: load_fn(h, c_string!("PyUnicode_FromString"))?,
        py_object_isinstance: load_fn(h, c_string!("PyObject_IsInstance"))?,
        py_long_as_longlong: load_fn(h, c_string!("PyLong_AsLongLong"))?,
        py_unicode_as_utf8_and_size: load_fn(h, c_string!("PyUnicode_AsUTF8AndSize"))?,
        py_object_str: load_fn(h, c_string!("PyObject_Str"))?,
        py_object_call: load_fn(h, c_string!("PyObject_Call"))?,
        py_object_getattr_string: load_fn(h, c_string!("PyObject_GetAttrString"))?,
        py_tuple_new: load_fn(h, c_string!("PyTuple_New"))?,
        py_tuple_set_item: load_fn(h, c_string!("PyTuple_SetItem"))?,
        py_list_size: load_fn(h, c_string!("PyList_Size"))?,
        py_list_get_item: load_fn(h, c_string!("PyList_GetItem"))?,
        py_tuple_size: load_fn(h, c_string!("PyTuple_Size"))?,
        py_tuple_get_item: load_fn(h, c_string!("PyTuple_GetItem"))?,
        py_dict_set_item_string: load_fn(h, c_string!("PyDict_SetItemString"))?,
        py_import_importmodule: load_fn(h, c_string!("PyImport_ImportModule"))?,
        py_module_new: load_fn(h, c_string!("PyModule_New"))?,
        py_module_get_dict: load_fn(h, c_string!("PyModule_GetDict"))?,
        py_run_string: load_fn(h, c_string!("PyRun_String"))?,
        py_err_fetch: load_fn(h, c_string!("PyErr_Fetch"))?,
        py_err_clear: load_fn(h, c_string!("PyErr_Clear"))?,
        py_thread_state_set_async_exc: load_fn(h, c_string!("PyThreadState_SetAsyncExc"))?,
        py_thread_get_thread_ident: load_fn(h, c_string!("PyThread_get_thread_ident"))?,
        py_finalize_ex: load_fn(h, c_string!("Py_FinalizeEx"))?,
    })
}

/// The directory of a loaded library (Windows: `GetModuleFileNameA`; unix:
/// `dladdr`). Used to enrich the candidate search — the stable-ABI
/// forwarder (`python3.dll`) and the full `python3XX.dll` always live side
/// by side, so a PATH-loaded forwarder tells us exactly where the full DLL
/// is, without spawning python to ask it.
#[cfg(windows)]
fn loaded_library_dir(h: Handle) -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameA;
    let mut buf = [0u8; 1024];
    let n = unsafe { GetModuleFileNameA(h, buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 {
        return None;
    }
    std::path::Path::new(String::from_utf8_lossy(&buf[..n as usize]).as_ref())
        .parent()
        .map(|p| p.to_string_lossy().replace('/', "\\"))
}

#[cfg(unix)]
fn loaded_library_dir(h: Handle) -> Option<String> {
    extern "C" {
        fn dladdr(addr: *mut std::ffi::c_void, info: *mut DlInfo) -> c_int;
    }
    #[repr(C)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut std::ffi::c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut std::ffi::c_void,
    }
    let mut info = DlInfo {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    if unsafe { dladdr(h, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    let path = unsafe { CStr::from_ptr(info.dli_fname) }
        .to_string_lossy()
        .to_string();
    std::path::Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
}

/// Append `<dir>/python3*.dll` candidates (deduped) to the search list.
fn push_dir_candidates(candidates: &mut Vec<Vec<u8>>, dir: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dlls: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|s| s.starts_with("python3") && s.ends_with(".dll"))
        .collect();
    dlls.sort();
    for f in dlls {
        let cand = format!("{}\\{}\0", dir, f).into_bytes();
        if !candidates.contains(&cand) {
            candidates.push(cand);
        }
    }
}

fn load_api() -> Result<(PyApi, Handle), String> {
    let mut last_err = String::from("no python library found");
    // Pin the exact library (`ALLOY_PYTHON_DLL`, e.g. the full path to a
    // specific python311.dll) — for hosts that bundle a python runtime and
    // must not pick up whatever `python3.dll` PATH resolves first (a
    // second python install, a debugger's PATH injection, a venv). Tried
    // before every other candidate; a pin that fails to load or misses a
    // symbol falls through to the normal search.
    let mut candidates = Vec::new();
    if let Ok(pinned) = std::env::var("ALLOY_PYTHON_DLL") {
        if !pinned.is_empty() {
            let mut cand = pinned.into_bytes();
            if cand.last() != Some(&0) {
                cand.push(0);
            }
            candidates.push(cand);
        }
    }
    candidates.extend(candidate_libraries());
    let mut i = 0;
    while i < candidates.len() {
        let cand = candidates[i].clone();
        i += 1;
        let Ok(h) = open_library_candidate(&cand) else {
            continue;
        };
        match unsafe { try_load_api(h) } {
            Ok(api) => return Ok((api, h)),
            Err(e) => {
                last_err = format!(
                    "{} (from {})",
                    e,
                    String::from_utf8_lossy(&cand[..cand.len() - 1])
                );
                // 1. A PATH-loaded forwarder reveals the full DLL's
                //    directory — enrich in place, no python subprocess.
                let mut enriched = false;
                if let Some(dir) = loaded_library_dir(h) {
                    let before = candidates.len();
                    push_dir_candidates(&mut candidates, &dir);
                    enriched = candidates.len() > before;
                }
                // 2. Last resort: ask the python executable where it lives.
                if !enriched {
                    if let Some(dir) = python_home_dir() {
                        let dir = dir.replace('/', "\\");
                        push_dir_candidates(&mut candidates, &dir);
                    }
                }
                close_library(h);
            }
        }
    }
    Err(last_err)
}

/// Cache the builtins type objects (new refs; process-lifetime). Must run
/// with the GIL held, before the interpreter's threads are released.
unsafe fn load_builtin_types(api: &PyApi) -> Result<PyTypes, String> {
    let builtins = (api.py_import_importmodule)(c_string!("builtins").as_ptr());
    if builtins.is_null() {
        let msg = fetch_error(api);
        return Err(format!("cannot import builtins: {}", msg));
    }
    let get = |name: &CStr| -> Result<PyObj, String> {
        let o = (api.py_object_getattr_string)(builtins, name.as_ptr());
        if o.is_null() {
            (api.py_err_clear)();
            (api.py_dec_ref)(builtins);
            return Err(format!("builtins has no type {}", name.to_string_lossy()));
        }
        Ok(o)
    };
    let types = PyTypes {
        bool_ty: get(c_string!("bool"))?,
        int_ty: get(c_string!("int"))?,
        float_ty: get(c_string!("float"))?,
        str_ty: get(c_string!("str"))?,
        list_ty: get(c_string!("list"))?,
        tuple_ty: get(c_string!("tuple"))?,
        none_ty: get(c_string!("None"))?,
        keyboard_interrupt: get(c_string!("KeyboardInterrupt"))?,
    };
    // Publish the interrupt machinery for the watchdog thread (which has
    // no access to the runtime).
    let _ = INTERRUPT_STATE.get_or_init(|| InterruptState {
        set_async_exc: api.py_thread_state_set_async_exc,
        exc: types.keyboard_interrupt,
        gilstate_ensure: api.pygilstate_ensure,
        gilstate_release: api.pygilstate_release,
    });
    // The module ref is no longer needed (the type objects are new refs).
    (api.py_dec_ref)(builtins);
    Ok(types)
}

/// Stable-ABI type check via `PyObject_IsInstance`.
unsafe fn is_inst(api: &PyApi, _types: &PyTypes, o: PyObj, ty: PyObj) -> bool {
    (api.py_object_isinstance)(o, ty) != 0
}

// ---------------------------------------------------------------------------
// GIL + helpers
// ---------------------------------------------------------------------------

/// RAII GIL: every C API access happens with the GIL held.
///
/// Locking contract with the watchdog: `PyGILState_Ensure` only INSERTS this
/// thread's tstate into the interpreter's list (a benign race for the
/// watchdog's walk), but `PyGILState_Release` DELETES it — so the release
/// runs under [`TEARDOWN_LOCK`], serialized against the watchdog's fire
/// (which walks the same list). The lock is held only around the C calls,
/// never across the python round-trip, so a fire can interrupt a running
/// call; and it is never held while waiting for the GIL, so there is no
/// lock-ordering cycle.
struct Gil<'a> {
    api: &'a PyApi,
    state: PyGILState,
}

impl<'a> Gil<'a> {
    fn acquire(api: &'a PyApi) -> Gil<'a> {
        let state = unsafe { (api.pygilstate_ensure)() };
        Gil { api, state }
    }
}

impl Drop for Gil<'_> {
    fn drop(&mut self) {
        let _lock = TEARDOWN_LOCK.lock().unwrap_or_else(|g| g.into_inner());
        unsafe { (self.api.pygilstate_release)(self.state) }
    }
}

/// Run `code` (file-input or eval) with `globals`/`locals`; on error, fetch
/// and format the exception. Returns a NEW reference (the result object).
unsafe fn run_string(
    api: &PyApi,
    code: &str,
    start: c_int,
    globals: PyObj,
    locals: PyObj,
) -> Result<PyObj, String> {
    let c = CString::new(code).map_err(|_| "python source contains NUL".to_string())?;
    let r = (api.py_run_string)(c.as_ptr(), start, globals, locals);
    if r.is_null() {
        Err(fetch_error(api))
    } else {
        Ok(r)
    }
}

/// Format the current (fetched-and-cleared) exception like the child's
/// `except Exception as e: str(e)` path.
unsafe fn fetch_error(api: &PyApi) -> String {
    let (msg, _) = fetch_error_ex(api, None);
    msg
}

/// Like [`fetch_error`], but also reports whether the exception is a
/// `KeyboardInterrupt` (the cooperative timeout's signal). `types` may be
/// `None` when only the message matters.
unsafe fn fetch_error_ex(api: &PyApi, types: Option<&PyTypes>) -> (String, bool) {
    let mut typ: PyObj = std::ptr::null_mut();
    let mut val: PyObj = std::ptr::null_mut();
    let mut tb: PyObj = std::ptr::null_mut();
    (api.py_err_fetch)(&mut typ, &mut val, &mut tb);
    let is_ki = match types {
        Some(types) => {
            let val_ki = !val.is_null() && is_inst(api, types, val, types.keyboard_interrupt);
            // A `PyThreadState_SetAsyncExc` raise carries a NULL value, and
            // the fetched `typ` is the exact class object we handed to it —
            // pointer identity, since a class is not an instance of itself.
            let typ_ki = !typ.is_null()
                && (typ == types.keyboard_interrupt
                    || is_inst(api, types, typ, types.keyboard_interrupt));
            val_ki || typ_ki
        }
        None => false,
    };
    let msg = if !val.is_null() {
        obj_str(api, val)
    } else if !typ.is_null() {
        obj_str(api, typ)
    } else {
        "python exception".to_string()
    };
    for p in [typ, val, tb] {
        if !p.is_null() {
            (api.py_dec_ref)(p);
        }
    }
    (msg, is_ki)
}

unsafe fn obj_str(api: &PyApi, o: PyObj) -> String {
    let s = (api.py_object_str)(o);
    if s.is_null() {
        (api.py_err_clear)();
        return "<unprintable>".to_string();
    }
    let out = py_utf8(api, s);
    (api.py_dec_ref)(s);
    out
}

unsafe fn py_utf8(api: &PyApi, o: PyObj) -> String {
    let mut len: isize = 0;
    let p = (api.py_unicode_as_utf8_and_size)(o, &mut len);
    if p.is_null() {
        (api.py_err_clear)();
        return String::new();
    }
    let bytes = std::slice::from_raw_parts(p as *const u8, len.max(0) as usize);
    String::from_utf8_lossy(bytes).to_string()
}

/// Escape an error message exactly like the child bootstrap's
/// `str(e).replace('\\',"\\\\").replace('\n',"\\n").replace(' ',"\\s")` —
/// the VM's drain path unescapes it, so both backends must frame identically.
fn escape_err(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(' ', "\\s")
}

/// Port of the child's `_enc`: bool/int/float/str/None/list|tuple → wire
/// tokens. `s:` lengths are byte lengths of the utf-8 payload, matching
/// `len(b)` on the child side.
unsafe fn enc_wire(rt: &PyRuntime, o: PyObj) -> String {
    let api = &rt.api;
    if o == rt.types.none_ty {
        return "v".to_string();
    }
    if is_inst(api, &rt.types, o, rt.types.bool_ty) {
        let b = (api.py_long_as_longlong)(o) != 0;
        return format!("b:{}", if b { "1" } else { "0" });
    }
    if is_inst(api, &rt.types, o, rt.types.int_ty) {
        let s = obj_str(api, o);
        return format!("n:{}", s);
    }
    if is_inst(api, &rt.types, o, rt.types.float_ty) {
        let s = obj_str(api, o);
        return format!("n:{}", s);
    }
    if is_inst(api, &rt.types, o, rt.types.str_ty) {
        let s = py_utf8(api, o);
        return format!("s:{}:{}", s.len(), s);
    }
    if is_inst(api, &rt.types, o, rt.types.list_ty) || is_inst(api, &rt.types, o, rt.types.tuple_ty)
    {
        let is_list = is_inst(api, &rt.types, o, rt.types.list_ty);
        let n = if is_list {
            (api.py_list_size)(o)
        } else {
            (api.py_tuple_size)(o)
        };
        let mut out = format!("a:{}", n);
        for i in 0..n {
            let item = if is_list {
                (api.py_list_get_item)(o, i)
            } else {
                (api.py_tuple_get_item)(o, i)
            };
            if !item.is_null() {
                out.push_str(&enc_wire(rt, item));
            }
        }
        return out;
    }
    let s = obj_str(api, o);
    format!("s:{}:{}", s.len(), s)
}

/// One wire arg token (`p:off`, `n:num`, `s:escaped`) → a new python object,
/// mirroring the child's arg parsing (ints stay ints, floats stay floats).
unsafe fn wire_arg(api: &PyApi, a: &str) -> Option<PyObj> {
    if let Some(rest) = a.strip_prefix("p:") {
        let off: i64 = rest.parse().ok()?;
        return Some((api.py_long_from_longlong)(off));
    }
    if let Some(rest) = a.strip_prefix("n:") {
        let is_int = !rest.is_empty()
            && (rest.chars().all(|c| c.is_ascii_digit())
                || (rest.starts_with('-')
                    && rest.len() > 1
                    && rest[1..].chars().all(|c| c.is_ascii_digit())));
        if is_int {
            return Some((api.py_long_from_longlong)(rest.parse().ok()?));
        }
        return Some((api.py_float_from_double)(rest.parse().ok()?));
    }
    if let Some(rest) = a.strip_prefix("s:") {
        let s = crate::python_sidecar::unescape(rest);
        let c = CString::new(s).ok()?;
        return Some((api.py_unicode_from_string)(c.as_ptr()));
    }
    None
}

// ---------------------------------------------------------------------------
// The per-file backend
// ---------------------------------------------------------------------------

/// One `.py` file's in-process interpreter state: a module dict in the
/// shared interpreter, plus the segment base/cap its `buf` helpers point at.
/// All access is GIL-guarded (the worker thread locks the owning mutex and
/// holds the GIL for the whole round trip), so the raw module pointer is
/// `Send` across the pool's worker threads.
pub struct EmbedPython {
    funcs: Vec<String>,
    py_file: String,
    base: usize,
    cap: usize,
    module: PyObj,
    /// Per-call deadline (`ALLOY_PYTHON_TIMEOUT_MS`; 0 disables). Unlike the
    /// child's kill-based timeout, this is **cooperative**: a watchdog
    /// raises `KeyboardInterrupt` in the executing worker thread via
    /// `PyThreadState_SetAsyncExc` (targeting the OS thread id the worker
    /// publishes while its call is in flight), which only fires when the
    /// running python code reaches a bytecode boundary. A function blocked
    /// in a C call with the GIL released (`time.sleep`, I/O) is not
    /// interrupted until it returns to python.
    timeout: Duration,
}

// Safety: the only shared state is the CPython module pointer, and every use
// of it is serialized by the worker pool's mutex + the GIL.
unsafe impl Send for EmbedPython {}

impl EmbedPython {
    /// Import `py_file` into a fresh module dict and bind the segment
    /// helpers over the raw segment pointer. Mirrors the child's handshake
    /// (validates the file imports), so a broken import fails loudly here.
    pub fn start(
        shared_base: usize,
        shared_cap: usize,
        py_file: &str,
        timeout: Duration,
    ) -> Result<EmbedPython, String> {
        let rt = runtime()?;
        let api = &rt.api;
        let _g = Gil::acquire(api);
        let (module, funcs) = unsafe {
            let module = import_module(api, shared_base, shared_cap, py_file)?;
            let funcs = module_funcs(rt, module)?;
            (module, funcs)
        };
        // Registered only after the import fully succeeded — a failed import
        // never holds a module reference.
        LIVE_BACKENDS.fetch_add(1, Ordering::Relaxed);
        Ok(EmbedPython {
            funcs,
            py_file: py_file.to_string(),
            base: shared_base,
            cap: shared_cap,
            module,
            timeout,
        })
    }

    /// Top-level callable names (the module object gets one native each),
    /// including the segment helpers — exactly the child's `dir(mod)` list.
    pub fn funcs(&self) -> &[String] {
        &self.funcs
    }

    /// One request/response round-trip, in-process, with the cooperative
    /// per-call timeout (see [`EmbedPython::timeout`]). Every path — parse
    /// errors included — stands the watchdog down and flushes any raced
    /// pending interrupt so it cannot leak into the next call.
    pub fn call_line(&mut self, line: &str) -> String {
        // The caller (the pool worker thread) holds this sidecar's mutex for
        // the whole round trip, and teardown/reload nulls `module` under the
        // SAME mutex — so if it is already null here, the sidecar was shut
        // down while a queued request was still in flight (the worker can be
        // parked in `recv()` when the pool is torn down and pick up a
        // remaining request after the module was released). Dereferencing it
        // would be a use-after-free (a NULL-deref in `PyObject_GetAttrString`
        // under parallel load). The child backend survives this shape because
        // its pipes hit EOF and return an error; this returns the equivalent
        // error, and the VM's burst check re-runs the call on the fresh pool.
        if self.module.is_null() {
            return "err python sidecar is not running".to_string();
        }
        let rt = match runtime() {
            Ok(r) => r.clone(),
            Err(e) => return format!("err python embed unavailable: {}", e),
        };
        let api = &rt.api;
        let gil = Gil::acquire(api);
        let timeout = self.timeout;
        // Fresh slate: a stale exception from a previous call would
        // otherwise poison the next one.
        unsafe { (api.py_err_clear)() };

        // A fresh per-call token for the arm/cancel pair.
        let tid = unsafe { (api.py_thread_get_thread_ident)() } as usize;
        let gen = GEN_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
        if !timeout.is_zero() {
            watchdog_arm(tid, gen, Instant::now() + timeout);
        }

        let resp = self.call_line_inner(api, &rt, line, gen);

        if !timeout.is_zero() {
            watchdog_cancel(gen);
            // Clear any fire recorded after the call returned (e.g. the
            // deadline expired as the result was being encoded) so it can
            // never misreport a LATER call on this thread.
            fired_gen(gen);
            // Execute a trivial snippet to flush any pending interrupt;
            // errors here are discarded.
            unsafe {
                let dict = (api.py_module_get_dict)(self.module);
                let _ = run_string(api, "pass", 257, dict, dict);
            }
        }
        // `gil` drops here; the GILState tstate deletion runs under the
        // teardown lock (see [`Gil`]).
        drop(gil);
        resp
    }

    /// The actual round trip (no timeout bookkeeping). Returns the wire
    /// response; a failure whose exception is `KeyboardInterrupt` AND whose
    /// interrupt flag fired is reported as the cooperative timeout.
    fn call_line_inner(&self, api: &PyApi, rt: &PyRuntime, line: &str, gen: usize) -> String {
        unsafe {
            let mut parts = line.split(' ');
            let _tag = parts.next();
            let Some(name) = parts.next() else {
                return "err bad-request".to_string();
            };
            let nargs: usize = match parts.next().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => return "err bad-request".to_string(),
            };
            let arg_tokens: Vec<&str> = parts.take(nargs).collect();
            let tuple = (api.py_tuple_new)(nargs as isize);
            for (i, t) in arg_tokens.iter().enumerate() {
                let Some(obj) = wire_arg(api, t) else {
                    (api.py_dec_ref)(tuple);
                    return "err bad-request".to_string();
                };
                // PyTuple_SetItem steals the reference.
                (api.py_tuple_set_item)(tuple, i as isize, obj);
            }
            let cname = match CString::new(name) {
                Ok(c) => c,
                Err(_) => {
                    (api.py_dec_ref)(tuple);
                    return "err bad-request".to_string();
                }
            };
            let callable = (api.py_object_getattr_string)(self.module, cname.as_ptr());
            if callable.is_null() {
                (api.py_err_clear)();
                (api.py_dec_ref)(tuple);
                return format!("err '{}' is not a function of the python module", name);
            }
            let res = (api.py_object_call)(callable, tuple, std::ptr::null_mut());
            (api.py_dec_ref)(tuple);
            (api.py_dec_ref)(callable);
            if res.is_null() {
                let (msg, is_ki) = fetch_error_ex(api, Some(&rt.types));
                if is_ki && fired_gen(gen) {
                    // Our cooperative interrupt: same surface as the child's
                    // kill-timeout message (the drain path just unescapes).
                    "err python call timed out (embed cooperative interrupt)".to_string()
                } else {
                    format!("err {}", escape_err(&msg))
                }
            } else {
                let enc = enc_wire(rt, res);
                (api.py_dec_ref)(res);
                format!("ok {}", enc)
            }
        }
    }

    /// Re-import the file fresh (a `.py` reload): drop the old module dict
    /// and exec the current source, in place — no process spawn.
    pub fn restart(&mut self) -> Result<(), String> {
        let rt = runtime()?;
        let api = &rt.api;
        let _g = Gil::acquire(api);
        unsafe {
            if !self.module.is_null() {
                (api.py_dec_ref)(self.module);
            }
        }
        let (m, funcs) = unsafe {
            let m = import_module(api, self.base, self.cap, &self.py_file)?;
            let funcs = module_funcs(rt, m)?;
            (m, funcs)
        };
        self.module = m;
        self.funcs = funcs;
        Ok(())
    }

    /// Release the module reference (called at VM teardown). Idempotent:
    /// Drop and the pool teardown may both reach it, so the counter and the
    /// reference are released exactly once.
    pub fn shutdown(&mut self) {
        if self.module.is_null() {
            return;
        }
        if let Ok(rt) = runtime() {
            let api = &rt.api;
            let _g = Gil::acquire(api);
            unsafe { (api.py_dec_ref)(self.module) };
        }
        self.module = std::ptr::null_mut();
        // Defensive: never underflow (a stray late Drop after finalization
        // must not wedge the guard at usize::MAX).
        let _ =
            LIVE_BACKENDS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

impl Drop for EmbedPython {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Create the module dict, bind the segment helpers over the raw pointer,
/// and exec the file into it. Returns a NEW reference to the module.
unsafe fn import_module(
    api: &PyApi,
    base: usize,
    cap: usize,
    py_file: &str,
) -> Result<PyObj, String> {
    let m = (api.py_module_new)(c_string!("alloy_mod").as_ptr());
    if m.is_null() {
        return Err("PyModule_New failed".into());
    }
    let dict = (api.py_module_get_dict)(m);
    if dict.is_null() {
        (api.py_dec_ref)(m);
        return Err("PyModule_GetDict failed".into());
    }
    // Same helper surface as the child's bootstrap, but `buf` is a ctypes
    // array over the actual shared segment bytes — zero-copy by construction.
    let helpers = format!(
        "import ctypes, struct\n\
         buf = (ctypes.c_char * {}).from_address({})\n\
         def read_f32(p): return struct.unpack_from('<f', buf, p)[0]\n\
         def read_f64(p): return struct.unpack_from('<d', buf, p)[0]\n\
         def read_i32(p): return struct.unpack_from('<i', buf, p)[0]\n\
         def read_u8(p): return buf[p]\n\
         def read_bytes(p, n): return bytes(buf[p:p + n])\n\
         def write_f32(p, v): struct.pack_into('<f', buf, p, v)\n\
         def write_f64(p, v): struct.pack_into('<d', buf, p, v)\n\
         def write_i32(p, v): struct.pack_into('<i', buf, p, v)\n\
         def write_u8(p, v): buf[p] = v & 0xff\n",
        cap, base
    );
    if let Err(e) = run_string(api, &helpers, 257, dict, dict) {
        (api.py_dec_ref)(m);
        return Err(format!("segment helper setup failed: {}", e));
    }
    let src = match std::fs::read_to_string(py_file) {
        Ok(s) => s,
        Err(e) => {
            (api.py_dec_ref)(m);
            return Err(format!("cannot read {}: {}", py_file, e));
        }
    };
    if let Err(e) = run_string(api, &src, 257, dict, dict) {
        (api.py_dec_ref)(m);
        return Err(format!("failed to import {}: {}", py_file, e));
    }
    Ok(m)
}

/// `[n for n in dir(_m) if callable(getattr(_m, n)) and not n.startswith('_')]`
/// — the same list the child's handshake emits, computed in-process.
unsafe fn module_funcs(rt: &PyRuntime, m: PyObj) -> Result<Vec<String>, String> {
    let api = &rt.api;
    let dict = (api.py_module_get_dict)(m);
    (api.py_dict_set_item_string)(dict, c_string!("_m").as_ptr(), m);
    let code = "[n for n in dir(_m) if callable(getattr(_m, n)) and not n.startswith('_')]";
    let list = run_string(api, code, 258, dict, dict)?;
    let mut out = Vec::new();
    if is_inst(api, &rt.types, list, rt.types.list_ty) {
        let n = (api.py_list_size)(list);
        for i in 0..n {
            let item = (api.py_list_get_item)(list, i);
            if !item.is_null() && is_inst(api, &rt.types, item, rt.types.str_ty) {
                out.push(py_utf8(api, item));
            }
        }
    }
    (api.py_dec_ref)(list);
    Ok(out)
}
