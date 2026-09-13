use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Create a pre-sized temp file to back the segment (read/write, created
/// exclusively so concurrent VMs never collide on the same path).
///
/// The name carries a process-wide counter as well as the clock, and
/// creation retries a few times, so two VMs in the same process (parallel
/// tests, a server's per-request VMs) can never race onto one path — a
/// collision would silently fall back to an anonymous segment, which breaks
/// the python sidecar's zero-copy file mapping.
fn create_temp_segment(capacity: usize) -> Result<(std::fs::File, std::path::PathBuf), String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let pid = std::process::id();
    let mut last_err = None;
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("alloy_shm_{}_{}_{}.tmp", pid, nanos, n);
        let path = std::env::temp_dir().join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                file.set_len(capacity as u64).map_err(|e| e.to_string())?;
                return Ok((file, path));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "cannot create shared segment temp file".to_string()))
}

/// Wall-clock milliseconds (the sweep's rate-limit and age clock).
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How old an `alloy_shm_*.tmp` orphan must be before the startup sweep
/// deletes it. The pid-liveness check is the primary guard (a live process's
/// segment is never touched); the age is belt-and-suspenders against pid
/// recycling — a freshly-started process that happens to hold a recycled pid
/// has a brand-new segment file, so anything older than this is not it.
const ORPHAN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Rate-limit the orphan sweep to once per interval per process, so a
/// process that creates many segments (parallel tests, worker VMs) scans the
/// temp dir once, not once per segment.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Last sweep time (wall-clock ms); 0 means never swept, so the first
/// `SidecarMemory::new` in a process always sweeps.
static LAST_SWEEP_MS: AtomicU64 = AtomicU64::new(0);

/// Result of one orphan sweep: how many stale `alloy_shm_*.tmp` segment
/// files were reclaimed and their total bytes — a proxy for how much disk
/// long-ago crashes leaked (each file is one crashed run's segment, sized
/// to its capacity at creation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepStats {
    pub files: usize,
    pub bytes: u64,
}

/// The most recent sweep's result (0/0 before the first sweep in this
/// process). Kept so any caller can query what startup reclaimed without
/// re-scanning the temp dir (the sweep itself is rate-limited).
static LAST_SWEEP_FILES: AtomicUsize = AtomicUsize::new(0);
static LAST_SWEEP_BYTES: AtomicU64 = AtomicU64::new(0);

/// The most recent orphan sweep's result: files reclaimed and total bytes.
/// Read this (or the startup report) to spot segment files leaked by
/// long-ago crashes — a nonzero count on a healthy machine means some run
/// died without its `Drop` ever removing the file.
pub fn last_orphan_sweep() -> SweepStats {
    SweepStats {
        files: LAST_SWEEP_FILES.load(Ordering::Relaxed),
        bytes: LAST_SWEEP_BYTES.load(Ordering::Relaxed),
    }
}

/// Run the orphan sweep **now**, bypassing the rate limit — for a
/// long-running server that wants to reclaim crashed-run segments between
/// requests (or an operator script) without waiting out the 60s window.
/// The same safety rules as the automatic sweep apply: a live process's
/// segment is never touched (pid-liveness on Unix, open-file semantics on
/// Windows) and files younger than [`ORPHAN_GRACE`] are kept, so calling
/// this freely is safe. Returns what was reclaimed, updates the
/// [`last_orphan_sweep`] view, and advances the rate-limit clock so the
/// automatic sweep doesn't re-scan the temp dir right after.
///
/// The VM exposes this to scripts as the global native `sweepSegments()`.
pub fn sweep_segments_now() -> SweepStats {
    let stats = sweep_stale_segments_inner(&std::env::temp_dir());
    LAST_SWEEP_FILES.store(stats.files, Ordering::Relaxed);
    LAST_SWEEP_BYTES.store(stats.bytes, Ordering::Relaxed);
    LAST_SWEEP_MS.store(wall_ms(), Ordering::Relaxed);
    stats
}

/// Is a process with `pid` currently alive? Conservative on unknown
/// platforms (returns true, so nothing is ever deleted). Shared by the
/// segment sweep (a live segment is never touched) and the python-sidecar
/// orphan watchdog in the VM crate (children of a dead parent are reaped).
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0): 0 → exists and signalable; EPERM → exists but owned by
    // someone else. Anything else (ESRCH) means the pid is free.
    unsafe { libc::kill(pid as i32, 0) == 0 }
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// On Windows the sweep guards live segments via open-file semantics (the
// delete fails); the pid probe is used by the watchdog for orphaned
// sidecar children.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Access denied means the process exists but can't be queried.
            return std::io::Error::last_os_error().raw_os_error() == Some(5);
        }
        CloseHandle(h);
        true
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Startup cleanup pass: delete `alloy_shm_*.tmp` files left behind by
/// crashed runs (their process is gone, so `Drop` never removed them). A
/// file is deleted only when BOTH (a) the pid embedded in its name is not a
/// live process — a live segment is never touched — and (b) it is older than
/// [`ORPHAN_GRACE`], guarding pid-recycling races. Rate-limited to once per
/// [`SWEEP_INTERVAL`] per process; called at the top of `SidecarMemory::new`.
fn sweep_stale_segments() {
    let now = wall_ms();
    let last = LAST_SWEEP_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SWEEP_INTERVAL.as_millis() as u64 {
        return;
    }
    // Compare-and-swap so concurrent VMs (parallel tests) sweep only once.
    if LAST_SWEEP_MS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let stats = sweep_stale_segments_inner(&std::env::temp_dir());
    LAST_SWEEP_FILES.store(stats.files, Ordering::Relaxed);
    LAST_SWEEP_BYTES.store(stats.bytes, Ordering::Relaxed);
    // Rate-limited to once per interval, so this fires at most once per
    // sweep — operators see a one-line report only when there is something
    // to know (a clean startup stays silent).
    if stats.files > 0 {
        eprintln!(
            "[alloy] reclaimed {} orphaned shared-segment file(s) from crashed runs ({} bytes total)",
            stats.files, stats.bytes
        );
    }
}

/// The actual sweep over one directory, split out so tests can target a
/// scratch dir without tripping the process-wide rate limit. Returns how
/// many files were actually deleted and their total bytes (only successful
/// removes count — on Windows a delete of a live segment fails, and that
/// file is not a swept orphan).
fn sweep_stale_segments_inner(dir: &std::path::Path) -> SweepStats {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return SweepStats::default();
    };
    let now = wall_ms();
    let mut stats = SweepStats::default();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("alloy_shm_") else {
            continue;
        };
        if !rest.ends_with(".tmp") {
            continue;
        }
        // Unix: a live process's segment must never be unlinked (unlink
        // succeeds on open files), so require the pid embedded in the name
        // (alloy_shm_{pid}_{nanos}_{counter}.tmp — pid is the first field)
        // to be dead. Windows needs no pid guard: deletion of an open file
        // fails with a sharing violation, so the OS itself protects live
        // segments — and a pid-liveness check there would only leak orphans
        // whose pid was recycled by an unrelated process (the delete attempt
        // is the real guard).
        #[cfg(unix)]
        {
            let Some(pid) = rest.split('_').next().and_then(|p| p.parse::<u32>().ok()) else {
                continue;
            };
            if pid_alive(pid) {
                continue;
            }
        }
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
        if age_ms < ORPHAN_GRACE.as_millis() as u64 {
            continue;
        }
        if std::fs::remove_file(e.path()).is_ok() {
            stats.files += 1;
            stats.bytes += meta.len();
        }
    }
    stats
}

/// Map the segment file into this process (MAP_SHARED): the pages are the
/// file, so a child sidecar that opens the same path sees every write with
/// zero serialization.
#[cfg(unix)]
fn map_file(file: &std::fs::File, capacity: usize) -> Result<*mut u8, String> {
    use std::os::unix::io::AsRawFd;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            capacity,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(ptr as *mut u8)
}

#[cfg(unix)]
fn unmap_file(ptr: *mut u8, capacity: usize) {
    unsafe { libc::munmap(ptr as *mut libc::c_void, capacity); }
}

#[cfg(windows)]
fn map_file(file: &std::fs::File, capacity: usize) -> Result<*mut u8, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };
    // Pass both dwords of capacity: the high 32 bits go in dwMaximumSizeHigh,
    // the low 32 bits in dwMaximumSizeLow. Without this, segments > 4 GB
    // would silently truncate.
    let size_high = (capacity as u64 >> 32) as u32;
    let size_low = capacity as u32;
    let mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READWRITE,
            size_high,
            size_low,
            std::ptr::null(),
        )
    };
    if mapping.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, capacity) };
    unsafe { CloseHandle(mapping) };
    if view.Value.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(view.Value as *mut u8)
}

#[cfg(windows)]
fn unmap_file(ptr: *mut u8, _capacity: usize) {
    use windows_sys::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr as *mut core::ffi::c_void });
    }
}

pub struct SidecarMemory {
    ptr: NonNull<u8>,
    capacity: usize,
    write_offset: AtomicUsize,
    read_offset: AtomicUsize,
    /// Open handle to the backing file (kept so the mapping stays valid and
    /// the path stays unique while the segment lives). `Some` exactly when
    /// the segment is file-backed and shareable with a child sidecar.
    file: Option<std::fs::File>,
    /// Path of the backing file; passed to sidecar processes via env so they
    /// can map the exact same segment.
    path: Option<std::path::PathBuf>,
}

unsafe impl Send for SidecarMemory {}
unsafe impl Sync for SidecarMemory {}

impl SidecarMemory {
    pub fn new(capacity: usize) -> Self {
        // Startup pass: reclaim segment files orphaned by crashed runs before
        // creating this VM's own (a live segment is never touched — only
        // files whose pid is dead and whose age exceeds the grace period).
        sweep_stale_segments();
        // Prefer a file-backed segment: a child sidecar process (Python, C,
        // another binary) maps the same file, so a pointer handed across is
        // zero-copy shared memory — the PRD's polyglot pillar. When the temp
        // dir or the OS mapping fails, fall back to an anonymous allocation
        // (functionally identical in-process; the bridge reports the segment
        // as non-shareable).
        if let Ok((file, path)) = create_temp_segment(capacity) {
            if let Ok(ptr) = map_file(&file, capacity) {
                return Self {
                    ptr: NonNull::new(ptr).unwrap(),
                    capacity,
                    write_offset: AtomicUsize::new(0),
                    read_offset: AtomicUsize::new(0),
                    file: Some(file),
                    path: Some(path),
                };
            }
        }
        let layout = std::alloc::Layout::from_size_align(capacity, 16)
            .expect("invalid shared memory layout");
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            capacity,
            write_offset: AtomicUsize::new(0),
            read_offset: AtomicUsize::new(0),
            file: None,
            path: None,
        }
    }

    pub fn raw_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Path of the backing file, when the segment is file-backed (shareable
    /// with a child sidecar process via its own mapping of the same file).
    pub fn file_path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    pub fn write(&self, data: &[u8]) -> Result<usize, SharedMemoryError> {
        let len = data.len();
        // CAS loop to avoid TOCTOU: claim space atomically.
        loop {
            let offset = self.write_offset.load(Ordering::Acquire);
            if offset + len > self.capacity {
                return Err(SharedMemoryError::Overflow {
                    requested: len,
                    available: self.capacity.saturating_sub(offset),
                });
            }
            if self.write_offset.compare_exchange_weak(offset, offset + len, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                unsafe {
                    let dst = self.ptr.as_ptr().add(offset);
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, len);
                }
                return Ok(offset);
            }
        }
    }

    pub fn read(&self, offset: usize, len: usize) -> Result<&[u8], SharedMemoryError> {
        if offset + len > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len });
        }

        unsafe {
            let ptr = self.ptr.as_ptr().add(offset);
            Ok(std::slice::from_raw_parts(ptr, len))
        }
    }

    /// Mutable read into the shared segment.
    ///
    /// Takes `&mut self` (not `&self`) to prevent overlapping mutable
    /// references: Rust's aliasing rules require exclusive access for
    /// `&mut [u8]`, so the borrow checker enforces that no other reference
    /// into the segment is live while this slice exists.
    pub fn read_mut(&mut self, offset: usize, len: usize) -> Result<&mut [u8], SharedMemoryError> {
        if offset + len > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len });
        }

        // SAFETY: `&mut self` guarantees exclusive access. The bounds check
        // above ensures `offset + len <= capacity`, so the pointer arithmetic
        // stays within the mapped region.
        unsafe {
            let ptr = self.ptr.as_ptr().add(offset);
            Ok(std::slice::from_raw_parts_mut(ptr, len))
        }
    }

    /// Reserve `size` bytes in the segment (8-byte aligned) and advance the
    /// write cursor. Used for raw shared buffers.
    pub fn bump(&self, size: usize) -> Result<usize, SharedMemoryError> {
        loop {
            let offset = self.write_offset.load(Ordering::Acquire);
            let aligned = (offset + 7) & !7;
            if aligned + size > self.capacity {
                return Err(SharedMemoryError::Overflow {
                    requested: size,
                    available: self.capacity.saturating_sub(aligned),
                });
            }
            let new_off = aligned + size;
            if self.write_offset.compare_exchange_weak(offset, new_off, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Ok(aligned);
            }
        }
    }

    pub fn allocate_float32_array(&self, values: &[f32]) -> Result<usize, SharedMemoryError> {
        let byte_len = std::mem::size_of_val(values);
        loop {
            let offset = self.write_offset.load(Ordering::Acquire);
            let aligned = (offset + 3) & !3;
            if aligned + byte_len > self.capacity {
                return Err(SharedMemoryError::Overflow {
                    requested: byte_len,
                    available: self.capacity.saturating_sub(aligned),
                });
            }
            let new_off = aligned + byte_len;
            if self.write_offset.compare_exchange_weak(offset, new_off, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                unsafe {
                    let dst = self.ptr.as_ptr().add(aligned);
                    let src = values.as_ptr() as *const u8;
                    std::ptr::copy_nonoverlapping(src, dst, byte_len);
                }
                return Ok(aligned);
            }
        }
    }

    /// Generic typed-array allocation shared by the f32/f64/i32 entry points.
    fn allocate_typed<T>(&self, values: &[T]) -> Result<usize, SharedMemoryError> {
        let byte_len = std::mem::size_of_val(values);
        let align = std::mem::align_of::<T>().max(4);
        loop {
            let offset = self.write_offset.load(Ordering::Acquire);
            let aligned = (offset + align - 1) & !(align - 1);
            if aligned + byte_len > self.capacity {
                return Err(SharedMemoryError::Overflow {
                    requested: byte_len,
                    available: self.capacity.saturating_sub(aligned),
                });
            }
            let new_off = aligned + byte_len;
            if self.write_offset.compare_exchange_weak(offset, new_off, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                unsafe {
                    let dst = self.ptr.as_ptr().add(aligned);
                    let src = values.as_ptr() as *const u8;
                    std::ptr::copy_nonoverlapping(src, dst, byte_len);
                }
                return Ok(aligned);
            }
        }
    }

    /// Generic typed-slice read with alignment + bounds validation.
    fn get_typed_slice<T>(&self, offset: usize, count: usize) -> Result<&[T], SharedMemoryError> {
        let byte_len = count * std::mem::size_of::<T>();
        if !offset.is_multiple_of(std::mem::align_of::<T>()) {
            return Err(SharedMemoryError::Unaligned { offset });
        }
        if offset + byte_len > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len: byte_len });
        }
        unsafe {
            let ptr = self.ptr.as_ptr().add(offset) as *const T;
            Ok(std::slice::from_raw_parts(ptr, count))
        }
    }

    pub fn allocate_float64_array(&self, values: &[f64]) -> Result<usize, SharedMemoryError> {
        self.allocate_typed(values)
    }

    pub fn allocate_int32_array(&self, values: &[i32]) -> Result<usize, SharedMemoryError> {
        self.allocate_typed(values)
    }

    /// Write a scalar value directly through the shared pointer.
    fn write_typed<T: Copy>(&self, offset: usize, v: T) -> Result<(), SharedMemoryError> {
        let byte_len = std::mem::size_of::<T>();
        if !offset.is_multiple_of(std::mem::align_of::<T>()) {
            return Err(SharedMemoryError::Unaligned { offset });
        }
        if offset + byte_len > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len: byte_len });
        }
        unsafe {
            let ptr = self.ptr.as_ptr().add(offset) as *mut T;
            std::ptr::write(ptr, v);
        }
        Ok(())
    }

    pub fn get_float64_slice(&self, offset: usize, count: usize) -> Result<&[f64], SharedMemoryError> {
        self.get_typed_slice(offset, count)
    }

    pub fn get_int32_slice(&self, offset: usize, count: usize) -> Result<&[i32], SharedMemoryError> {
        self.get_typed_slice(offset, count)
    }

    /// Zero-copy scalar view: read a `f32`/`f64`/`i32`/`u8` at a raw offset.
    /// This is the shared-heap side of the polyglot story — a sidecar process
    /// (Python via ctypes, another Rust binary, etc.) reads/writes the exact
    /// same bytes, so a JS Float64Array written here is visible to the sidecar
    /// with no serialization or copy.
    pub fn read_float32(&self, offset: usize) -> Result<f32, SharedMemoryError> {
        self.get_typed_slice::<f32>(offset, 1).map(|s| s[0])
    }

    pub fn read_float64(&self, offset: usize) -> Result<f64, SharedMemoryError> {
        self.get_typed_slice::<f64>(offset, 1).map(|s| s[0])
    }

    pub fn read_int32(&self, offset: usize) -> Result<i32, SharedMemoryError> {
        self.get_typed_slice::<i32>(offset, 1).map(|s| s[0])
    }

    pub fn read_uint8(&self, offset: usize) -> Result<u8, SharedMemoryError> {
        if offset + 1 > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len: 1 });
        }
        unsafe { Ok(*self.ptr.as_ptr().add(offset)) }
    }

    pub fn write_float32(&self, offset: usize, v: f32) -> Result<(), SharedMemoryError> {
        self.write_typed(offset, v)
    }

    pub fn write_float64(&self, offset: usize, v: f64) -> Result<(), SharedMemoryError> {
        self.write_typed(offset, v)
    }

    pub fn write_int32(&self, offset: usize, v: i32) -> Result<(), SharedMemoryError> {
        self.write_typed(offset, v)
    }

    pub fn write_uint8(&self, offset: usize, v: u8) -> Result<(), SharedMemoryError> {
        if offset + 1 > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len: 1 });
        }
        unsafe {
            *self.ptr.as_ptr().add(offset) = v;
        }
        Ok(())
    }

    pub fn get_float32_slice(&self, offset: usize, count: usize) -> Result<&[f32], SharedMemoryError> {
        let byte_len = count * std::mem::size_of::<f32>();
        if !offset.is_multiple_of(std::mem::align_of::<f32>()) {
            return Err(SharedMemoryError::Unaligned { offset });
        }
        if offset + byte_len > self.capacity {
            return Err(SharedMemoryError::OutOfBounds { offset, len: byte_len });
        }

        unsafe {
            let ptr = self.ptr.as_ptr().add(offset) as *const f32;
            Ok(std::slice::from_raw_parts(ptr, count))
        }
    }

    pub fn reset(&self) {
        self.write_offset.store(0, Ordering::Release);
        self.read_offset.store(0, Ordering::Release);
    }

    pub fn used(&self) -> usize {
        self.write_offset.load(Ordering::Acquire)
    }

    pub fn available(&self) -> usize {
        self.capacity - self.used()
    }
}

#[derive(Debug)]
pub enum SharedMemoryError {
    Overflow { requested: usize, available: usize },
    OutOfBounds { offset: usize, len: usize },
    Unaligned { offset: usize },
}

impl std::fmt::Display for SharedMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overflow { requested, available } => {
                write!(f, "shared memory overflow: requested {} bytes, {} available", requested, available)
            }
            Self::OutOfBounds { offset, len } => {
                write!(f, "shared memory out of bounds: offset {}, len {}", offset, len)
            }
            Self::Unaligned { offset } => {
                write!(f, "shared memory unaligned access at offset {}", offset)
            }
        }
    }
}

impl std::error::Error for SharedMemoryError {}

impl Drop for SidecarMemory {
    fn drop(&mut self) {
        match self.file.take() {
            Some(file) => {
                // Unmap, close the handle, then delete the file (Windows
                // cannot delete a file with an open mapping/handle). Any
                // sidecar still holding it is a child of this VM and dies
                // with it (the VM's sidecar field drops before this one).
                unmap_file(self.ptr.as_ptr(), self.capacity);
                drop(file);
                if let Some(p) = self.path.take() {
                    // The children were killed and joined before this drops,
                    // but a child's handle can take a moment to fully release
                    // on Windows. Retry briefly with backoff so a clean drop
                    // never leaves a fresh orphan for the startup sweep to
                    // find — the sweep stays as a safety net for crashes.
                    for attempt in 0..5u32 {
                        if std::fs::remove_file(&p).is_ok() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(
                            10 * (attempt as u64 + 1),
                        ));
                    }
                }
            }
            None => {
                let layout = std::alloc::Layout::from_size_align(self.capacity, 16).unwrap();
                unsafe {
                    std::alloc::dealloc(self.ptr.as_ptr(), layout);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sidecar_write_read() {
        let mem = SidecarMemory::new(4096);
        let data = b"hello sidecar";
        let offset = mem.write(data).unwrap();
        let read = mem.read(offset, data.len()).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn startup_sweep_deletes_orphaned_segment_files() {
        // A pid that is guaranteed dead: spawn a throwaway child, reap it,
        // then use its pid for the fake orphan. On the rare chance the pid
        // is recycled in the microseconds before we check, skip (the test
        // needs a provably-dead pid to be meaningful).
        let dead_pid = {
            let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
                .arg(if cfg!(windows) { "/C" } else { "-c" })
                .arg("exit 0")
                .spawn()
                .expect("spawn throwaway child");
            let pid = child.id();
            child.wait().expect("reap throwaway child");
            pid
        };
        if pid_alive(dead_pid) {
            eprintln!("skip: helper pid {} was recycled immediately", dead_pid);
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "alloy_sweep_test_{}_{}",
            std::process::id(),
            dead_pid
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // A stale orphan for the dead pid: named exactly like a crashed
        // run's segment, aged past the grace period.
        let orphan = dir.join(format!("alloy_shm_{}_123456789_0.tmp", dead_pid));
        std::fs::write(&orphan, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        {
            // A write handle: setting file times needs FILE_WRITE_ATTRIBUTES,
            // which a read-only handle lacks on Windows.
            let f = std::fs::File::options().write(true).open(&orphan).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .expect("age the orphan past the grace period");
        }
        // A live-looking file (this process's own pid, fresh) must survive.
        let live = dir.join(format!("alloy_shm_{}_987654321_0.tmp", std::process::id()));
        std::fs::write(&live, b"x").unwrap();
        // A fresh orphan (dead pid, young mtime) must survive the grace check.
        let young = dir.join(format!("alloy_shm_{}_111111111_0.tmp", dead_pid));
        std::fs::write(&young, b"x").unwrap();
        // An aged file whose pid is LIVE (simulating a crashed run's pid
        // recycled by an unrelated process): Windows reclaims it (nothing
        // holds it open, so the delete succeeds); Unix keeps it (unlink on an
        // open file would succeed, so a live pid is treated as live).
        let recycled = dir.join(format!("alloy_shm_{}_555555555_0.tmp", std::process::id()));
        std::fs::write(&recycled, b"x").unwrap();
        {
            let f = std::fs::File::options().write(true).open(&recycled).unwrap();
            let _ = f.set_times(std::fs::FileTimes::new().set_modified(old));
        }
        let stats = sweep_stale_segments_inner(&dir);
        assert!(!orphan.exists(), "dead-pid orphan must be swept");
        assert!(live.exists(), "live-pid segment must never be swept");
        assert!(young.exists(), "young orphan must survive the grace period");
        #[cfg(windows)]
        assert!(
            !recycled.exists(),
            "an unheld file with a live (recycled) pid must be swept on Windows"
        );
        #[cfg(unix)]
        assert!(
            recycled.exists(),
            "a live pid must protect its file on Unix (unlink would succeed on an open file)"
        );
        // Accounting: exactly the files that were deleted, and their total
        // bytes (each fake orphan is 1 byte). Windows reclaims the aged
        // unheld file with the live (recycled) pid too; Unix keeps it.
        #[cfg(windows)]
        assert_eq!(
            stats,
            SweepStats { files: 2, bytes: 2 },
            "Windows sweeps the dead-pid orphan and the aged unheld recycled-pid file"
        );
        #[cfg(unix)]
        assert_eq!(
            stats,
            SweepStats { files: 1, bytes: 1 },
            "Unix sweeps only the dead-pid orphan (the live pid protects its file)"
        );
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&young);
        let _ = std::fs::remove_file(&recycled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float32_array() {
        let mem = SidecarMemory::new(4096);
        let values = vec![1.0f32, 2.0, 3.0, 4.0];
        let offset = mem.allocate_float32_array(&values).unwrap();
        let slice = mem.get_float32_slice(offset, values.len()).unwrap();
        assert_eq!(slice, &values[..]);
    }

    #[test]
    fn test_float32_alignment_after_bytes() {
        let mem = SidecarMemory::new(4096);
        // Unaligned byte write shifts the cursor to 5.
        mem.write(b"hello").unwrap();
        let values = vec![1.5f32, 2.5, 3.5];
        let offset = mem.allocate_float32_array(&values).unwrap();
        assert_eq!(offset % 4, 0, "float32 array must be 4-byte aligned");
        let slice = mem.get_float32_slice(offset, values.len()).unwrap();
        assert_eq!(slice, &values[..]);
    }

    #[test]
    fn test_float64_and_int32_arrays() {
        let mem = SidecarMemory::new(4096);
        let f64s = vec![1.5f64, -2.25, 3.125, 1e300];
        let off = mem.allocate_float64_array(&f64s).unwrap();
        assert_eq!(off % 8, 0);
        assert_eq!(mem.get_float64_slice(off, f64s.len()).unwrap(), &f64s[..]);

        let i32s = vec![1i32, -7, 42, 1 << 30];
        let off2 = mem.allocate_int32_array(&i32s).unwrap();
        assert_eq!(mem.get_int32_slice(off2, i32s.len()).unwrap(), &i32s[..]);
    }

    /// The zero-copy polyglot proof: one side ("JS") writes through the typed
    /// view, the other side ("the Python sidecar", simulated here with raw
    /// scalar views of the same segment) reads the exact bytes back — no
    /// serialization, no copy, just shared memory.
    #[test]
    fn test_sidecar_zero_copy_scalar_views() {
        let mem = SidecarMemory::new(4096);
        // JS side: allocate a Float64Array and write through its slice.
        let vals = vec![1.2345f64, 2.3456, 1.0];
        let off = mem.allocate_float64_array(&vals).unwrap();
        let js_view = mem.get_float64_slice(off, 3).unwrap();
        assert_eq!(js_view, &vals[..]);
        // Byte-level views agree on the underlying memory (before any write).
        let bytes = mem.read(off, 24).unwrap();
        assert_eq!(bytes, unsafe {
            std::slice::from_raw_parts(vals.as_ptr() as *const u8, 24)
        });
        // JS writes a scalar into the middle of the array zero-copy.
        mem.write_float64(off + 8, 42.0).unwrap();
        // Sidecar reads the same byte region with a raw scalar view.
        assert_eq!(mem.read_float64(off + 8).unwrap(), 42.0);
        assert_eq!(mem.read_float64(off).unwrap(), 1.2345);
        // i32 + u8 scalar round-trips.
        mem.write_int32(off, -12345).unwrap();
        assert_eq!(mem.read_int32(off).unwrap(), -12345);
        mem.write_uint8(off + 100, 0xAB).unwrap();
        assert_eq!(mem.read_uint8(off + 100).unwrap(), 0xAB);
    }

    #[test]
    fn test_bump_aligned() {
        let mem = SidecarMemory::new(4096);
        let off = mem.bump(10).unwrap();
        assert_eq!(off % 8, 0);
        assert_eq!(mem.used(), 10);
        // Next bump lands on an 8-byte boundary even after the odd-sized write.
        let off2 = mem.bump(4).unwrap();
        assert_eq!(off2 % 8, 0);
        assert!(off2 >= off + 10);
    }
}
