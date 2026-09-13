use std::collections::HashMap as StdHashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hashbrown::HashMap;

use alloy_core::value::{Value, VmHost};
use crate::bytecode::Program;
use crate::compiler::Compiler;

use super::core::{FRAME_BUDGET, MAX_COMPILED_MODULES, MAX_PY_TRACKED_MODULES, ModuleGlobals, STACK_SIZE, Vm};
use super::ops_async::ThrowResult;
use super::stack::CallFrame;

/// Per-module reload tracking for `.ajs` source modules (incremented once
/// per reload, compared against each thread's copy to detect that it moved).
/// `.py` sidecar modules use `burst` + `last_reload_ms`:
/// reloads arriving within [`PY_RELOAD_COALESCE_MS`] of the previous one are
/// the same **burst** and fold into it — only the first reload of a burst
/// bumps `burst`, so a rapid succession of reloads while calls are in flight
/// triggers exactly one pool rebuild / re-run instead of a cascade.
#[derive(Default)]
pub(crate) struct ModuleGen {
    /// `.ajs` reload generation: bumped once per reload, compared against the
    /// value each thread's cached copy was loaded at.
    pub(crate) gen: AtomicU64,
    /// `.py`: bumped when a NEW burst of reloads starts (the previous reload
    /// was more than the coalescing window ago). Pool and in-flight call
    /// stamps compare against this, so one burst = at most one abort.
    pub(crate) burst: AtomicU64,
    /// `.py`: wall-clock ms of the most recent reload, the burst-window
    /// anchor (`now - last_reload_ms > window` starts a new burst).
    pub(crate) last_reload_ms: AtomicU64,
}

/// Process-wide compiled-module cache shared by the main VM and every spawn
/// worker. Values cannot cross threads (the arena heap is thread-local), so
/// what is shared is the *compiled program bytes* — compile once, load in
/// any VM — plus a per-path generation for race-free `reload()`. Each thread
/// still runs a module's top-level code in its own isolated globals (Node's
/// worker model: per-worker module state); a reload invalidates every
/// thread's copy at its next require.
#[derive(Default)]
pub(crate) struct SharedModuleRegistry {
    /// canonical path -> (compiled program bytes, generation cell).
    pub(crate) compiled: Mutex<StdHashMap<String, (Arc<[u8]>, Arc<ModuleGen>)>>,
}

impl SharedModuleRegistry {
    /// Load-or-compile a module's program bytes exactly once per generation.
    /// The compile closure runs OUTSIDE the registry lock: holding it across a
    /// slow compile would serialize every module load process-wide (a slow
    /// compile blocking unrelated requires). The double-check on re-entry
    /// keeps compile-once semantics — a racing thread's duplicate compile is
    /// simply discarded in favor of the winner's cached bytes.
    pub(crate) fn get_or_compile(
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
    pub(crate) fn invalidate(&self, canon: &str) -> bool {
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
pub(crate) const PY_RELOAD_COALESCE_MS: u64 = 250;

/// The `.py` reload coalescing window in milliseconds, read from
/// `ALLOY_PYTHON_RELOAD_MS` (default [`PY_RELOAD_COALESCE_MS`]). Read **per
/// call** rather than cached: reloads are rare (a process-env lookup here is
/// free) and it lets tests and operators change the window without a
/// restart. A value of 0 keeps the window semantics (only same-millisecond
/// reloads fold) — it does not disable folding, just narrows it.
pub(crate) fn py_reload_coalesce_ms() -> u64 {
    std::env::var("ALLOY_PYTHON_RELOAD_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(PY_RELOAD_COALESCE_MS)
}

/// Wall-clock milliseconds, the burst-window clock. Monotonicity isn't
/// required (a clock step only shifts where a burst boundary lands) but
/// saturation is: the first reload ever compares against 0, which is always
/// a new burst.
pub(crate) fn now_ms() -> u64 {
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
pub(crate) struct SharedPyRegistry {
    /// canonical .py path -> reload state cell.
    pub(crate) gen: Mutex<StdHashMap<String, Arc<ModuleGen>>>,
}

impl SharedPyRegistry {
    /// The reload-state cell for `canon`, creating it on first use (a VM's
    /// import records the current value; a reload folds into it). Bounded:
    /// past [`MAX_PY_TRACKED_MODULES`] unique paths, everything but the
    /// freshly-created cell is dropped — a later call on an evicted path
    /// recreates the cell (burst id restarts at 0, at worst one extra
    /// re-import after a reload, never stale bytes).
    pub(crate) fn cell(&self, canon: &str) -> Arc<ModuleGen> {
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
    pub(crate) fn burst(&self, canon: &str) -> u64 {
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
    pub(crate) fn invalidate(&self, canon: &str) -> (bool, bool) {
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

impl Vm {
    /// True when `path` is an explicit `.py` import or reload. A directory
    /// or extension-less specifier is always a JS module, so `require` and
    /// `reload` never spawn a python child for a javascript file.
    pub(crate) fn is_py_specifier(path: &str) -> bool {
        path.trim_end().to_ascii_lowercase().ends_with(".py")
    }

    /// Resolve a `.py` sidecar specifier like `require` does — relative to
    /// the requiring file's directory, with the `.py` extension supplied if
    /// omitted — returning the canonical path when the file exists. Used by
    /// `reload('./x.py')` and `require('./x.py')` (workers load python
    /// modules this way) and to canonicalize import keys.
    pub(crate) fn resolve_py_path(&self, path: &str) -> Option<String> {
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

    pub(crate) fn resolve_module_path(&self, path: &str) -> Result<String, String> {
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
    pub(crate) fn resolve_file_or_dir(&self, base: &std::path::Path) -> Option<String> {
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
    pub(crate) fn read_package_main(pj: &std::path::Path) -> Option<String> {
        let s = std::fs::read_to_string(pj).ok()?;
        let idx = s.find("\"main\"")?;
        let rest = &s[idx + 6..];
        let colon = rest.find(':')?;
        let after = rest[colon + 1..].trim_start();
        let q = after.strip_prefix('"')?;
        let end = q.find('"')?;
        Some(q[..end].to_string())
    }

    pub fn reload_module(&mut self, path: &str) -> bool {
        if self.require_builtin(path).is_some() {
            return false;
        }
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
        let had_shared = self.registry.invalidate(&canon);
        had_local || had_shared
    }

    /// `require('fs')` / `require('alloy:fs')` return the builtin module
    /// object (Node's builtin modules), cached so it's a singleton.
    pub(crate) fn require_builtin(&mut self, path: &str) -> Option<Value> {
        const BUILTINS: &[&str] = &[
            "fs", "http", "memory", "channel", "Promise", "Date", "Math", "JSON",
            "Number", "Object", "Array", "String", "console", "setTimeout",
            "setInterval", "clearTimeout", "clearInterval", "queueMicrotask",
            "parseInt", "parseFloat", "isNaN", "Error", "TypeError", "RangeError",
            "ReferenceError", "SyntaxError", "EvalError", "URIError", "fetchSync",
            "crypto", "URL", "encodeURIComponent", "decodeURIComponent",
            "encodeURI", "decodeURI", "btoa", "atob",
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
        self.require_cache
            .insert(key, (v.clone(), Arc::new(ModuleGen::default()), 0));
        Some(v)
    }

    pub fn require_module(&mut self, path: &str) -> Value {
        // Node-style builtins win over node_modules lookups.
        if let Some(b) = self.require_builtin(path) {
            return b;
        }
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
        if let Some((exports, gen, gen_at)) = self.require_cache.get(&canon) {
            if gen.gen.load(Ordering::Relaxed) == *gen_at {
                return exports.clone();
            }
            self.require_cache.remove(&canon);
        }
        if self.requiring.contains(&canon) {
            self.throw_exception(Value::string(format!(
                "Error: Circular require of '{}'",
                path
            )));
            return Value::undefined();
        }
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
        let names = self.programs[pid as usize].globals.clone();
        let mut globals = Vec::with_capacity(names.len());
        let mut defined = Vec::with_capacity(names.len());
        for name in &names {
            let v = self.seed_global_named(name);
            defined.push(!v.is_undefined());
            globals.push(v);
        }
        for (_, binding) in &exports_pairs {
            if let Some(i) = names.iter().position(|n| n == binding) {
                let v = std::mem::replace(&mut globals[i], Value::undefined());
                globals[i] = Value::cell(v);
            }
        }
        self.modules.insert(pid, ModuleGlobals { globals, defined });
        self.requiring.push(canon.clone());
        let saved_dir = self.current_dir.take();
        self.current_dir = std::path::Path::new(&canon)
            .parent()
            .map(|p| p.to_path_buf());

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
            generator_id: None,
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

    pub(crate) fn load_module_bytes(canon: &str, requested: &str) -> Result<Arc<[u8]>, String> {
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
}
