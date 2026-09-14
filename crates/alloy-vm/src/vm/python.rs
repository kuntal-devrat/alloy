use std::sync::{mpsc, Arc, Mutex};

use crate::python_sidecar::{PyArg, PythonSidecar};
use alloy_core::value::{PromiseState, PromiseStatus, Value, VmHost};

use super::core::{Vm, PYTHON_CALL_TIMEOUT_MS, PYTHON_POOL_SIZE};

/// One queued call to a sidecar's dedicated worker thread.
pub(crate) struct PyRequest {
    /// Call id — the promise it settles lives in `python_inflight_calls`.
    pub(crate) id: u64,
    /// Prebuilt wire request line (owned data; no arena references cross
    /// threads, so the worker can never touch the thread-local heap).
    pub(crate) line: String,
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
pub(crate) struct InflightPyCall {
    pub(crate) promise: Value,
    /// Shared burst id at queue time — the stale check compares against it.
    pub(crate) burst: u64,
    /// Prebuilt wire request line, reused verbatim for the re-run.
    pub(crate) line: String,
}

/// A `.py` file's worker **pool**: child processes (each with its own worker
/// thread and request queue) so same-file calls run in parallel — a single
/// child is single-threaded, but K children give K-way concurrency. Children
/// spawn **lazily**: the first at import time, a new one only when every
/// existing child has an in-flight call (capped by `max`), so idle servers
/// pay for one python process, not K.
pub(crate) struct PythonWorker {
    /// One request queue per child; requests go to the least-busy child.
    pub(crate) senders: Vec<mpsc::Sender<PyRequest>>,
    /// In-flight request count per child (decremented by completions).
    pub(crate) busy: Vec<usize>,
    /// Round-robin tiebreak for equal-busy children (VM thread only).
    pub(crate) next: usize,
    /// Shared sidecar handles, one per child: the workers lock them per
    /// round-trip (responses can't be misattributed); the VM locks them at
    /// teardown to kill every child before joining.
    pub(crate) sidecars: Vec<Arc<Mutex<PythonSidecar>>>,
    /// Child pids, mirror of `sidecars` — teardown kills by pid first so a
    /// worker blocked in a read never holds up the join.
    pub(crate) pids: Vec<u32>,
    /// Top-level function names of the imported file (for the module object).
    pub(crate) funcs: Vec<String>,
    /// Worker threads, joined at VM teardown so every child is dead before
    /// the shared segment's backing file is unmapped/deleted.
    pub(crate) handles: Vec<std::thread::JoinHandle<()>>,
    /// Pool cap (`ALLOY_PYTHON_POOL`): growth stops here.
    pub(crate) max: usize,
    /// Shared-segment backing file + capacity, for spawning grown children.
    pub(crate) path: String,
    pub(crate) cap: usize,
    /// Raw base pointer of the shared segment (embed mode accesses it
    /// directly; the child mode only needs `path`).
    pub(crate) base: usize,
    /// The imported `.py` file, for spawning grown children.
    pub(crate) py_file: String,
    /// Completions channel clone for grown workers: `(src, child, id, resp)`.
    pub(crate) complete: mpsc::Sender<(String, usize, u64, String)>,
    /// Shared `.py` reload burst this pool's children were spawned in. A
    /// reload that starts a NEW burst on any thread bumps it; the next call
    /// on this VM compares and re-imports when it moved (kills these
    /// children, builds a fresh pool). Reloads folded into the same burst
    /// leave it alone — in-flight children survive, so a burst of reloads
    /// causes at most one rebuild.
    pub(crate) burst: u64,
    /// Per-pool per-call timeout override (from `Vm::set_python_timeout`);
    /// `None` falls back to `ALLOY_PYTHON_TIMEOUT_MS` / the built-in
    /// default. VM-scoped so one VM's short deadline never leaks into pools
    /// that other VMs (or other tests) spawn concurrently.
    pub(crate) timeout: Option<std::time::Duration>,
}

impl PythonWorker {
    /// Queue `req` to the least-busy child, growing the pool when every child
    /// is busy (up to `max`). The caller must have incremented nothing yet.
    pub(crate) fn send(&mut self, req: PyRequest) -> bool {
        if self.senders.is_empty() {
            return false;
        }
        if self.least_busy().is_none_or(|i| self.busy[i] > 0) && self.senders.len() < self.max {
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
    pub(crate) fn least_busy(&self) -> Option<usize> {
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
    pub(crate) fn grow(&mut self) -> bool {
        let timeout = self.timeout.unwrap_or_else(|| {
            std::time::Duration::from_millis(
                std::env::var("ALLOY_PYTHON_TIMEOUT_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(PYTHON_CALL_TIMEOUT_MS),
            )
        });
        let sidecar =
            match PythonSidecar::start(&self.path, self.cap, self.base, &self.py_file, timeout) {
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
                    let resp = {
                        let mut s = match worker_sidecar.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        s.call_line_timeout(&req.line, timeout)
                    };
                    let _ = complete.send((src.clone(), idx, req.id, resp));
                }
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

/// Translate one JS call argument onto the sidecar wire: a shared-segment
/// buffer (or a raw in-segment pointer number like `buf.ptr`) becomes a
/// segment offset (`p:`), which the Python side indexes into its mmap
/// zero-copy. Everything else passes as a number or string.
pub(crate) fn python_arg(v: &Value, base: usize, cap: usize) -> PyArg {
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

/// Kill one pool's children, stop their sidecars, and join the worker
/// threads (reload path and VM teardown).
pub(crate) fn shutdown_python_pool(w: &mut PythonWorker) {
    for pid in &w.pids {
        if *pid != 0 {
            crate::python_sidecar::kill_pid_export(*pid);
        }
    }
    for sc in &w.sidecars {
        if let Ok(mut s) = sc.lock() {
            s.shutdown();
        }
    }
    w.senders.clear();
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    handles.append(&mut w.handles);
    for h in handles {
        let _ = h.join();
    }
}

impl Vm {
    pub(crate) fn ensure_python_current(&mut self, src: &str) -> bool {
        let burst_now = self.py_registry.burst(src);
        let stale = match self.python_workers.get(src) {
            Some(w) => w.burst != burst_now,
            None => false,
        };
        if stale {
            self.shutdown_python_worker(src);
            self.python_modules.remove(src);
        }
        if self.python_workers.get(src).is_none() {
            return self.start_python_worker(src, burst_now);
        }
        true
    }

    pub(crate) fn start_python_worker(&mut self, src: &str, burst: u64) -> bool {
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
        worker.funcs = worker
            .sidecars
            .first()
            .and_then(|sc| sc.lock().ok())
            .map(|s| s.funcs().to_vec())
            .unwrap_or_default();
        self.python_workers.insert(src.to_string(), worker);
        true
    }

    pub(crate) fn python_module(&mut self, src: &str) -> Value {
        let canon = self.resolve_py_path(src).unwrap_or_else(|| src.to_string());
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

    pub(crate) fn vm_python_call(
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
        if !self.ensure_python_current(src) {
            return reject(format!("python module '{}' is not loaded", src));
        }
        let wire: Vec<PyArg> = args.iter().map(|a| python_arg(a, base, cap)).collect();
        let line = PythonSidecar::build_line(func, &wire);
        let promise = self.new_promise();
        let id = self.next_cont_id;
        self.next_cont_id += 1;
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
            self.python_workers.remove(src);
            self.python_inflight = self.python_inflight.saturating_sub(1);
            self.python_inflight_calls.remove(&id);
            return reject("python sidecar is not running".to_string());
        }
        promise
    }

    pub(crate) fn python_send(&mut self, src: &str, req: PyRequest) -> bool {
        match self.python_workers.get_mut(src) {
            Some(w) => w.send(req),
            None => false,
        }
    }

    pub(crate) fn drain_python_completions(&mut self) -> bool {
        let mut any = false;
        while let Ok((src, idx, id, resp)) = self.python_rx.try_recv() {
            any = true;
            self.python_inflight = self.python_inflight.saturating_sub(1);
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
                if !self.python_send(
                    &src,
                    PyRequest {
                        id,
                        line: call.line.clone(),
                    },
                ) {
                    self.python_inflight = self.python_inflight.saturating_sub(1);
                    self.python_inflight_calls.remove(&id);
                    if let Some(pr) = call.promise.as_promise() {
                        self.reject_promise(
                            pr,
                            Value::string("python sidecar is not running".to_string()),
                        );
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

    pub(crate) fn shutdown_python_workers(&mut self) {
        let mut workers: Vec<PythonWorker> = std::mem::take(&mut self.python_workers)
            .into_values()
            .collect();
        for w in &mut workers {
            shutdown_python_pool(w);
        }
    }

    pub(crate) fn shutdown_python_worker(&mut self, src: &str) {
        if let Some(mut w) = self.python_workers.remove(src) {
            shutdown_python_pool(&mut w);
        }
    }
}
