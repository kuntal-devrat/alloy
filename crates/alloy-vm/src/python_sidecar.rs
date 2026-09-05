//! Zero-copy polyglot bridge: a persistent CPython sidecar process.
//!
//! The JS VM owns a file-backed [`SidecarMemory`] segment (mmap'd). The
//! sidecar opens the exact same file and mmaps it, so a JS buffer's bytes are
//! readable/writable from Python with zero serialization — the PRD's "pointer
//! handoff" story. Function *calls* cross a small line-based control channel
//! (function name + arguments); the data never does. Arguments are segment
//! offsets (`p:`), numbers, or strings; Python functions receive the raw
//! offset and read/write the segment through the `read_*`/`write_*` helpers
//! preloaded into their module namespace.
//!
//! One sidecar per imported `.py` file, kept alive for the VM's lifetime and
//! killed on drop. Startup performs a handshake that also validates the file
//! imports, so a broken import fails loudly at import time instead of at the
//! first call.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use alloy_core::value::Value;

/// Marker file naming the live python sidecar children of this process
/// (`alloy_py_sidecar_{child_pid}.tmp` in the temp dir). Each sidecar writes
/// one at spawn and removes it at shutdown; the orphan watchdog reaps
/// children whose recorded parent pid is dead (a leaked VM from a crashed or
/// detached run leaves both the child and its marker behind). Content:
/// `parent_pid\nsegment_path\n`.
fn marker_path(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("alloy_py_sidecar_{}.tmp", pid))
}

fn write_marker(pid: u32, parent: u32, shared_path: &str) {
    // Best effort: the watchdog only needs a hint, and a marker that fails
    // to write just means a crash is detected later (or not at all).
    let _ = std::fs::write(marker_path(pid), format!("{}\n{}\n", parent, shared_path));
}

fn remove_marker(pid: u32) {
    let _ = std::fs::remove_file(marker_path(pid));
}

/// Wall-clock milliseconds (the orphan scan's rate-limit clock).
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How often the orphan scan runs (once per process per interval) and how
/// old a marker must be before its child is reaped. The age guards against
/// pid recycling: a just-written marker is either a live child (skipped by
/// the live-parent check) or a brand-new orphan that will be reaped on the
/// next scan — only an aged marker whose recorded parent is dead is acted
/// on, so a recycled pid can never be killed.
const ORPHAN_SCAN_INTERVAL: Duration = Duration::from_secs(10);
const ORPHAN_MARKER_GRACE: Duration = Duration::from_secs(60);

/// Last orphan scan time (wall-clock ms); 0 means never scanned, so the
/// first `Vm` created in a process always scans.
static LAST_ORPHAN_SCAN_MS: AtomicU64 = AtomicU64::new(0);

/// Safety-net watchdog for hosts that leak VMs (e.g. a detached serve
/// thread): python sidecar children whose **parent process is dead** are
/// orphaned — a previous run crashed or exited without dropping its VMs, so
/// `Vm::drop` never killed them. They hold the shared-segment files open
/// (which blocks the startup sweep on Windows) and consume memory forever;
/// this pass kills them and removes their markers before the segment sweep
/// runs, so the leaked files become deletable. Called at the top of every
/// `Vm` construction, rate-limited to once per [`ORPHAN_SCAN_INTERVAL`].
///
/// Honest limitation: a leaked VM whose parent process is **still alive**
/// (a live host that abandoned it) is undetectable — Rust cannot observe
/// that an object was leaked. Those hosts must drop their VMs (the
/// stoppable serve thread is the supported path).
pub fn reap_orphaned_python_children() {
    let now = wall_ms();
    let last = LAST_ORPHAN_SCAN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < ORPHAN_SCAN_INTERVAL.as_millis() as u64 {
        return;
    }
    // Compare-and-swap so concurrent VMs scan only once per interval.
    if LAST_ORPHAN_SCAN_MS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let reaped = reap_orphaned_python_children_inner(&std::env::temp_dir(), now);
    if reaped > 0 {
        eprintln!(
            "[alloy] reaped {} orphaned python sidecar child(ren) from crashed runs",
            reaped
        );
    }
}

/// The actual scan over one directory, split out so tests can target a
/// scratch dir without tripping the process-wide rate limit. Returns how
/// many orphaned children were killed. For each `alloy_py_sidecar_*.tmp`
/// marker: a dead recorded child is just stale (marker removed); a live
/// child whose recorded parent is alive belongs to a live VM (skipped); a
/// live child whose parent is dead and whose marker is past the grace
/// period is an orphan and is killed.
fn reap_orphaned_python_children_inner(dir: &Path, now: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut reaped = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("alloy_py_sidecar_") else {
            continue;
        };
        let Some(child_pid) = rest.strip_suffix(".tmp").and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        // Content: `parent_pid\nsegment_path\n` — only the first line
        // matters for the orphan check.
        let Ok(content) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Some(parent_pid) = content.lines().next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        // A dead recorded child: the marker is stale (child crashed on its
        // own) — remove it regardless of the parent, so markers can never
        // accumulate.
        if !alloy_core::pid_alive(child_pid) {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        // Live child whose parent is alive: a live VM owns it (or an
        // undetectable in-process leak) — never touch it.
        if alloy_core::pid_alive(parent_pid) {
            continue;
        }
        // Orphan candidate: parent dead. Age-gate so a just-orphaned
        // child's pid can't have been recycled — the next scan reaps it.
        let Ok(meta) = e.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let age_ms = now.saturating_sub(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
        if age_ms < ORPHAN_MARKER_GRACE.as_millis() as u64 {
            continue;
        }
        kill_pid(child_pid);
        reaped += 1;
        let _ = std::fs::remove_file(e.path());
    }
    reaped
}

/// Kill a child process by pid without holding its `Child` handle. Used by
/// the per-call timeout: the worker may be blocked in a read on the child's
/// stdout while the sidecar's mutex is held by the helper thread, so the
/// kill must not need `&mut PythonSidecar`. The child's stdin/stdout then
/// hit EOF, which unblocks the reader and lets the sidecar be restarted.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    // Safety: killing our own spawned child by its recorded pid. The pid is
    // recycled only after the child is reaped (which happens on restart), so
    // the target is always the sidecar process.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // Safety: same reasoning as the unix path — this pid belongs to the
    // sidecar child we spawned.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn kill_pid(_pid: u32) {}

/// Public alias for the VM's teardown path, which kills every child by pid
/// without holding the sidecar mutex.
pub fn kill_pid_export(pid: u32) {
    kill_pid(pid);
}

/// One wire argument: a shared-segment offset, a number, or a string.
pub enum PyArg {
    /// Offset into the shared segment (what `buf.ptr` resolves to).
    Ptr(u64),
    Num(f64),
    Str(String),
}

/// Escape a string for the space-separated wire protocol: backslash, newline
/// and space are the only characters that would corrupt framing.
pub(crate) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(' ', "\\s")
}

pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('s') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Recursively decode a wire result into a JS value. Each element is
/// self-delimiting (no separators): `v` (undefined), `b:0|1`, `n:<float>`
/// (greedy float token), `s:<len>:<raw utf-8>`, `a:<count><elem>*` (array).
/// Must run on the VM thread: decoded strings allocate into the arena heap,
/// which is thread-local.
pub(crate) fn decode_wire(s: &[u8], i: &mut usize) -> Result<Value, String> {
    let peek = |i: &mut usize| -> Option<char> { s.get(*i).map(|&b| b as char) };
    match peek(i) {
        Some('v') => {
            *i += 1;
            Ok(Value::undefined())
        }
        Some('b') => {
            if s.get(*i + 1) == Some(&b':') {
                let v = s.get(*i + 2) == Some(&b'1');
                *i += 3;
                Ok(Value::bool(v))
            } else {
                Err("bad bool token".to_string())
            }
        }
        Some('n') => {
            *i += 2;
            let start = *i;
            while let Some(c) = peek(i) {
                if c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E') {
                    *i += 1;
                } else {
                    break;
                }
            }
            let txt = std::str::from_utf8(&s[start..*i])
                .map_err(|_| "non-utf8 number token".to_string())?;
            let f: f64 = txt
                .parse()
                .map_err(|_| format!("bad number token '{}'", txt))?;
            Ok(Value::number(f))
        }
        Some('s') => {
            *i += 2;
            let start = *i;
            while let Some(c) = peek(i) {
                if c.is_ascii_digit() {
                    *i += 1;
                } else {
                    break;
                }
            }
            let len: usize = std::str::from_utf8(&s[start..*i])
                .ok()
                .and_then(|t| t.parse().ok())
                .ok_or("bad string length token")?;
            if s.get(*i) != Some(&b':') {
                return Err("bad string token".to_string());
            }
            *i += 1;
            let bytes = s
                .get(*i..*i + len)
                .ok_or("string token truncated")?;
            *i += len;
            Ok(Value::string(String::from_utf8_lossy(bytes).to_string()))
        }
        Some('a') => {
            *i += 2;
            let start = *i;
            while let Some(c) = peek(i) {
                if c.is_ascii_digit() {
                    *i += 1;
                } else {
                    break;
                }
            }
            let count: usize = std::str::from_utf8(&s[start..*i])
                .ok()
                .and_then(|t| t.parse().ok())
                .ok_or("bad array count token")?;
            let mut elems = Vec::with_capacity(count);
            for _ in 0..count {
                elems.push(decode_wire(s, i)?);
            }
            Ok(Value::array(elems))
        }
        _ => Err(format!("python sidecar sent an unparseable result (offset {})", *i)),
    }
}

/// Bootstrap executed in the child: map the shared segment, load the imported
/// `.py` file, preload segment access helpers into its namespace, report its
/// top-level functions, then serve `call` requests over stdin.
const BOOTSTRAP: &str = r#"
import mmap
import os
import struct
import sys
import types

f = open(os.environ['ALLOY_SHM'], 'r+b')
buf = mmap.mmap(f.fileno(), int(os.environ['ALLOY_SHM_CAP']))
# Load the user module by reading the file directly and exec'ing it. Deliberately
# NOT importlib's SourceFileLoader: that consults __pycache__ and validates the
# cached .pyc against the source mtime truncated to whole seconds, so a rewrite
# within the same second (same byte size) would make a freshly spawned child
# re-import stale bytecode instead of the current file — breaking reload().
# Reading the file fresh guarantees a new child always runs the current source.
mod = types.ModuleType('alloy_mod')
mod.__file__ = os.environ['ALLOY_PY_FILE']
mod.__package__ = ''
with open(os.environ['ALLOY_PY_FILE'], 'rb') as _fh:
    _src = _fh.read()
exec(compile(_src, os.environ['ALLOY_PY_FILE'], 'exec'), mod.__dict__)

def read_f32(p):
    return struct.unpack_from('<f', buf, p)[0]
def read_f64(p):
    return struct.unpack_from('<d', buf, p)[0]
def read_i32(p):
    return struct.unpack_from('<i', buf, p)[0]
def read_u8(p):
    return buf[p]
def read_bytes(p, n):
    return bytes(buf[p:p + n])
def write_f32(p, v):
    struct.pack_into('<f', buf, p, v)
def write_f64(p, v):
    struct.pack_into('<d', buf, p, v)
def write_i32(p, v):
    struct.pack_into('<i', buf, p, v)
def write_u8(p, v):
    buf[p] = v & 0xff

# The user module's functions resolve globals in their own namespace, so the
# segment access helpers must live there too.
mod.read_f32 = read_f32
mod.read_f64 = read_f64
mod.read_i32 = read_i32
mod.read_u8 = read_u8
mod.read_bytes = read_bytes
mod.write_f32 = write_f32
mod.write_f64 = write_f64
mod.write_i32 = write_i32
mod.write_u8 = write_u8

def _emit(s):
    sys.stdout.write(s + '\n')
    sys.stdout.flush()

def _enc(v):
    if isinstance(v, bool):
        return 'b:' + ('1' if v else '0')
    if isinstance(v, int):
        return 'n:' + str(v)
    if isinstance(v, float):
        return 'n:' + repr(v)
    if isinstance(v, str):
        b = v.encode('utf-8')
        return 's:%d:' % len(b) + v
    if isinstance(v, (list, tuple)):
        return 'a:%d' % len(v) + ''.join(_enc(x) for x in v)
    if v is None:
        return 'v'
    b = str(v).encode('utf-8')
    return 's:%d:' % len(b) + b.decode('utf-8')

funcs = [n for n in dir(mod) if callable(getattr(mod, n)) and not n.startswith('_')]
_emit('funcs ' + ' '.join(funcs))

for line in sys.stdin:
    line = line.rstrip('\r\n')
    if not line:
        continue
    parts = line.split(' ')
    if parts[0] == 'quit':
        break
    if parts[0] != 'call':
        _emit('err bad-request')
        continue
    name = parts[1]
    nargs = int(parts[2])
    args = []
    for a in parts[3:3 + nargs]:
        if a.startswith('p:'):
            args.append(int(a[2:]))
        elif a.startswith('n:'):
            v = a[2:]
            args.append(int(v) if v.isdigit() or (v.startswith('-') and v[1:].isdigit()) else float(v))
        elif a.startswith('s:'):
            raw = a[2:]
            s = ''
            i = 0
            while i < len(raw):
                if raw[i] == '\\' and i + 1 < len(raw):
                    nxt = raw[i + 1]
                    if nxt == 'n':
                        s += '\n'
                    elif nxt == 's':
                        s += ' '
                    elif nxt == '\\':
                        s += '\\'
                    else:
                        s += '\\' + nxt
                    i += 2
                else:
                    s += raw[i]
                    i += 1
            args.append(s)
        else:
            args.append(a)
    try:
        r = getattr(mod, name)(*args)
        _emit('ok ' + _enc(r))
    except Exception as e:
        _emit('err ' + str(e).replace('\\', '\\\\').replace('\n', '\\n').replace(' ', '\\s'))
"#;

/// A live sidecar: the child process plus its control pipes.
pub struct ChildSidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Top-level callable names reported by the handshake (the module object
    /// gets one native per name).
    pub funcs: Vec<String>,
    /// Everything needed to respawn the child after a per-call timeout killed
    /// it: the shared segment's backing file, its capacity, and the module
    /// source path.
    shared_path: String,
    cap: usize,
    py_file: String,
}

impl ChildSidecar {
    /// Spawn the sidecar for `py_file` and handshake. `shared_path` is the
    /// path of the file backing the shared segment (from
    /// `SidecarMemory::file_path`).
    pub fn start(shared_path: &str, shared_cap: usize, py_file: &str) -> Result<ChildSidecar, String> {
        let python = std::env::var("ALLOY_PYTHON").unwrap_or_else(|_| {
            if cfg!(windows) { "python".to_string() } else { "python3".to_string() }
        });
        let mut cmd = Command::new(&python);
        cmd.arg("-u")
            .arg("-c")
            .arg(BOOTSTRAP)
            .env("ALLOY_SHM", shared_path)
            .env("ALLOY_SHM_CAP", shared_cap.to_string())
            .env("ALLOY_PY_FILE", py_file)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start python sidecar ({}): {}", python, e))?;
        let stdin = child.stdin.take().ok_or_else(|| "sidecar stdin unavailable".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "sidecar stdout unavailable".to_string())?;
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        let n = stdout
            .read_line(&mut line)
            .map_err(|e| format!("sidecar handshake read failed: {}", e))?;
        if n == 0 {
            return Err("python sidecar exited during startup (is the file valid Python?)".to_string());
        }
        let line = line.trim();
        let funcs = line
            .strip_prefix("funcs ")
            .map(|rest| rest.split_whitespace().map(|s| s.to_string()).collect())
            .ok_or_else(|| format!("python sidecar unexpected startup line: {}", line))?;
        // Register this child in the orphan watch: a marker naming the child
        // and its parent lets a later VM reap it if this run ever dies
        // without dropping its VMs.
        write_marker(child.id(), std::process::id(), shared_path);
        Ok(ChildSidecar {
            child,
            stdin,
            stdout,
            funcs,
            shared_path: shared_path.to_string(),
            cap: shared_cap,
            py_file: py_file.to_string(),
        })
    }

    /// The child's process id (used to kill it on a per-call timeout without
    /// holding the sidecar mutex).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Respawn the child after it was killed by a timeout: reap the old one
    /// and run the import handshake again (validates the file still loads).
    pub fn restart(&mut self) -> Result<(), String> {
        let old_pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_marker(old_pid);
        let fresh = ChildSidecar::start(&self.shared_path, self.cap, &self.py_file)?;
        *self = fresh;
        Ok(())
    }

    /// One request/response round-trip with a deadline. The blocking pipe I/O
    /// runs on a scoped helper thread while this thread waits up to `timeout`;
    /// on timeout the child is killed by pid (which unblocks the helper's
    /// read with EOF) and the child is restarted, so a hung python function
    /// can never park a request forever. The response is the raw wire line, or
    /// an `err …` line describing the timeout.
    pub fn call_line_timeout(&mut self, line: &str, timeout: Duration) -> String {
        let pid = self.child.id();
        let mut timed_out = false;
        let resp = std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            // Scoped thread: reborrows `self` (the caller's mutex guard) for
            // the blocking round-trip while this thread waits on the channel.
            // The borrow ends when the scope joins, freeing `self` for the
            // restart below.
            let this = &mut *self;
            s.spawn(move || {
                let r = this.call_line(line);
                let _ = tx.send(r);
            });
            match rx.recv_timeout(timeout) {
                Ok(r) => r,
                Err(_) => {
                    timed_out = true;
                    kill_pid(pid);
                    "err python call timed out (sidecar killed)".to_string()
                }
            }
        });
        if timed_out {
            // The scope joined the helper (its read hit EOF after the kill),
            // so the mutex guard is free again: respawn the child.
            if let Err(e) = self.restart() {
                return format!("err python sidecar restart failed: {}", e);
            }
        }
        resp
    }

    /// Build the wire request line for `func` and `args`. Pure and owned: it
    /// is constructed on the VM thread (args hold no arena references) and
    /// handed to a worker thread, which performs the blocking round-trip.
    pub fn build_line(func: &str, args: &[PyArg]) -> String {
        let mut line = format!("call {} {}", func, args.len());
        for a in args {
            match a {
                PyArg::Ptr(p) => line.push_str(&format!(" p:{}", p)),
                PyArg::Num(n) => line.push_str(&format!(" n:{}", n)),
                PyArg::Str(s) => line.push_str(&format!(" s:{}", escape(s))),
            }
        }
        line
    }

    /// Perform one blocking request/response round-trip against the sidecar
    /// and return the raw response line ("ok …" / "err …"). Runs on a worker
    /// thread; the caller holds this sidecar's mutex, so concurrent calls to
    /// the same file serialize (the child is single-threaded anyway).
    pub fn call_line(&mut self, line: &str) -> String {
        let wrote = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush());
        if wrote.is_err() {
            return "err sidecar write failed".to_string();
        }
        let mut resp = String::new();
        match self.stdout.read_line(&mut resp) {
            Ok(0) => "err python sidecar exited during call".to_string(),
            Ok(_) => resp.trim().to_string(),
            Err(e) => format!("err sidecar read failed: {}", e),
        }
    }
}

impl ChildSidecar {
    /// Stop the child: send `quit` (graceful) and kill it if needed, then
    /// deregister it from the orphan watch so no later VM mistakes a cleanly
    /// stopped child for a leaked one.
    pub fn shutdown(&mut self) {
        let pid = self.child.id();
        let _ = self.stdin.write_all(b"quit\n");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_marker(pid);
    }
}

impl Drop for ChildSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One `.py` file's python execution backend: either a subprocess child (the
/// default; killable per-call timeout, pool grows for same-file parallelism)
/// or an in-process CPython interpreter (`ALLOY_PYTHON_EMBED=1`; zero
/// process overhead, GIL-serialized, no kill). Both speak the same wire
/// protocol, so the VM's async pool, reload bursts, and promise settling are
/// backend-agnostic.
pub enum PythonSidecar {
    Child(ChildSidecar),
    Embed(crate::python_embed::EmbedPython),
}

impl PythonSidecar {
    /// Start a backend for `py_file`. Embed mode (when `ALLOY_PYTHON_EMBED`
    /// is set and the interpreter loads) gets the segment's raw base pointer
    /// for direct access; otherwise the child maps the segment's backing
    /// file. Embed init failures are logged once inside
    /// [`crate::python_embed`] and fall back to a child silently.
    pub fn start(
        shared_path: &str,
        shared_cap: usize,
        shared_base: usize,
        py_file: &str,
        timeout: Duration,
    ) -> Result<PythonSidecar, String> {
        if crate::python_embed::embed_enabled() {
            eprintln!("[alloy] python embed active for {}", py_file);
            return Ok(PythonSidecar::Embed(
                crate::python_embed::EmbedPython::start(shared_base, shared_cap, py_file, timeout)?,
            ));
        }
        Ok(PythonSidecar::Child(ChildSidecar::start(
            shared_path,
            shared_cap,
            py_file,
        )?))
    }

    /// Top-level callable names (the module object gets one native each).
    pub fn funcs(&self) -> &[String] {
        match self {
            PythonSidecar::Child(c) => &c.funcs,
            PythonSidecar::Embed(e) => e.funcs(),
        }
    }

    /// A process id usable for kill-on-timeout: the child's pid, or 0 in
    /// embed mode (nothing to kill — a hung function is not interruptible
    /// in-process).
    pub fn pid(&self) -> u32 {
        match self {
            PythonSidecar::Child(c) => c.pid(),
            PythonSidecar::Embed(_) => 0,
        }
    }

    /// Respawn/restart the backend (post-timeout or reload): a fresh child
    /// or an in-place re-import.
    pub fn restart(&mut self) -> Result<(), String> {
        match self {
            PythonSidecar::Child(c) => c.restart(),
            PythonSidecar::Embed(e) => e.restart(),
        }
    }

    /// One request/response round-trip with a deadline. In child mode a hung
    /// python function is killed at the deadline and the child respawned; in
    /// embed mode the deadline is ignored (the GIL cannot be interrupted) and
    /// the call just runs.
    pub fn call_line_timeout(&mut self, line: &str, timeout: Duration) -> String {
        match self {
            PythonSidecar::Child(c) => c.call_line_timeout(line, timeout),
            PythonSidecar::Embed(e) => e.call_line(line),
        }
    }

    /// Build the wire request line for `func` and `args` (backend-agnostic).
    pub fn build_line(func: &str, args: &[PyArg]) -> String {
        ChildSidecar::build_line(func, args)
    }

    /// Perform one blocking request/response round-trip and return the raw
    /// response line ("ok …" / "err …").
    pub fn call_line(&mut self, line: &str) -> String {
        match self {
            PythonSidecar::Child(c) => c.call_line(line),
            PythonSidecar::Embed(e) => e.call_line(line),
        }
    }

    /// Stop the backend: kill the child / release the module reference.
    pub fn shutdown(&mut self) {
        match self {
            PythonSidecar::Child(c) => c.shutdown(),
            PythonSidecar::Embed(e) => e.shutdown(),
        }
    }
}

impl Drop for PythonSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_escaping_round_trips() {
        for s in ["hello world", "line1\nline2", "back\\slash", "mixed \n\\ ", ""] {
            assert_eq!(unescape(&escape(s)), s);
        }
    }

    /// A helper process that stays alive long enough for the watchdog test
    /// (the `child` of a fake marker).
    fn spawn_helper() -> std::process::Child {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg("ping -n 30 127.0.0.1 >nul");
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.arg("-c").arg("sleep 30");
            c
        };
        cmd.spawn().expect("spawn helper")
    }

    fn age_past_grace(path: &std::path::Path) {
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        // Write handle: setting file times needs FILE_WRITE_ATTRIBUTES,
        // which a read-only handle lacks on Windows.
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("age the marker past the grace period");
    }

    /// The orphan watchdog: a marker naming a live child whose recorded
    /// parent is dead (a crashed/leaked run) and whose age exceeds the
    /// grace period → the child is killed and the marker removed. A live
    /// child of a live parent is never touched, and a stale marker (dead
    /// child) is removed even under a live parent.
    #[test]
    fn orphan_watchdog_reaps_children_of_dead_parents() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_orphan_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A provably-dead, NON-RECYCLABLE parent pid. A real dead pid from
        // spawn + reap is unreliable under heavy parallel load: the OS can
        // recycle it within milliseconds, and a recycled pid makes a real
        // orphan look like a live VM's child (the parent-liveness check
        // skips it and the kill never happens). `u32::MAX - 1` is far above
        // any real pid range, so `pid_alive` is always false on every
        // platform (Unix: beyond pid_max → ESRCH; Windows: OpenProcess
        // fails with ERROR_INVALID_PARAMETER) and the OS can never allocate
        // it.
        const DEAD_PID: u32 = u32::MAX - 1;

        // Case 1: orphan — live child, dead parent, aged marker → killed.
        let mut orphan = spawn_helper();
        let orphan_pid = orphan.id();
        let m1 = dir.join(format!("alloy_py_sidecar_{}.tmp", orphan_pid));
        std::fs::write(&m1, format!("{}\n/tmp/fake_segment\n", DEAD_PID)).unwrap();
        age_past_grace(&m1);

        // Case 2: live VM's child — live parent (this process), live child,
        // aged marker → untouched.
        let mut live = spawn_helper();
        let live_pid = live.id();
        let m2 = dir.join(format!("alloy_py_sidecar_{}.tmp", live_pid));
        std::fs::write(&m2, format!("{}\n/tmp/fake_segment\n", std::process::id())).unwrap();
        age_past_grace(&m2);

        // Case 3: stale marker — dead child, live parent → marker removed.
        // Same sentinel pid: a real recycled dead pid could be reused by a
        // live process, making the stale marker look live and survive.
        let m3 = dir.join(format!("alloy_py_sidecar_{}.tmp", DEAD_PID));
        std::fs::write(&m3, format!("{}\n/tmp/fake_segment\n", std::process::id())).unwrap();

        let reaped = reap_orphaned_python_children_inner(&dir, wall_ms());

        // The orphan was killed (try_wait flips to Some — retry briefly so
        // the OS has a moment to reap) and its marker removed.
        let mut status = None;
        for _ in 0..25 {
            status = orphan.try_wait().unwrap();
            if status.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(status.is_some(), "orphaned child must be killed by the watchdog");
        assert!(!m1.exists(), "orphan marker must be removed");
        assert_eq!(reaped, 1, "exactly the orphan is reaped");

        // The live VM's child survives (and is reaped by us so no leak).
        assert!(
            live.try_wait().unwrap().is_none(),
            "live child of a live parent must never be touched"
        );
        let _ = live.kill();
        let _ = live.wait();
        let _ = std::fs::remove_file(&m2);

        // The stale marker was removed despite the live parent.
        assert!(!m3.exists(), "stale marker for a dead child must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
