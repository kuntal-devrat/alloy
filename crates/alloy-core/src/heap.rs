//! Arena value heap: the GC-killer backing store for the JS value heap.
//!
//! Every heap-allocated JavaScript value (string, array, object) lives in an
//! [`ArenaHeap`]: the box is bump-allocated and never freed individually, and
//! teardown is one bulk free of the chunks at `ArenaHeap` drop — no per-object
//! `Rc` release, no per-object free. Clones of arena-backed values are plain
//! 8-byte bit copies (no reference-count traffic), and drops are no-ops.
//!
//! The arena must be reachable from `Value` construction, which happens in the
//! VM dispatch loop, native functions, the compiler's constant emission, and
//! standalone code. Rather than thread a heap handle through every call site,
//! the *active* heap is stored in a thread-local: whoever owns an allocation
//! context (the VM while `run()` executes, the compiler while it emits,
//! `Program::from_bytes` while it deserializes) installs a [`HeapGuard`], and
//! `Value::string`/`array`/`object` allocate from the current heap. Outside
//! any context the allocation falls back to a per-thread heap that lives for
//! the thread's lifetime, so standalone values (tests, module seeding) are
//! always safe — never freed, never dangling.

use crate::arena::ChunkedArena;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

/// Box payload kinds (the sweep uses them to drop contents correctly).
/// 0 = raw bytes / opaque (nothing to drop); 1 = `AString` box (bytes live in
/// the heap, nothing to drop); 2 = array box `RefCell<Vec<Value>>` (drop the
/// elements); 3 = object box `RefCell<ObjectData>` (drop the whole data —
/// shape `Rc` and property values).
pub const KIND_RAW: u64 = 0;
pub const KIND_STRING: u64 = 1;
pub const KIND_ARRAY: u64 = 2;
pub const KIND_OBJECT: u64 = 3;

/// Two-arena bump heap for JS heap values. Allocations go to the *young*
/// generation; the *old* generation holds values promoted across unit
/// boundaries (a script run, an HTTP request) by the escape-analysis pass.
/// The old generation is **non-copying**: the second-generation sweep
/// (major GC) marks live boxes and reclaims dead ones onto a free list that
/// later promotions reuse, so a long-running server that churns globals
/// reclaims old-gen garbage without ever copying the live set — a huge live
/// cache costs a mark + sweep, not a full relocation.
///
/// Every allocation (box or raw bytes) carries an 8-byte header `(size<<4)|kind`
/// so the sweeps can walk the arenas linearly and drop each dead box's
/// contents exactly once (releasing the `Rc`s it references).
#[derive(Debug)]
pub struct ArenaHeap {
    young: ChunkedArena,
    old: ChunkedArena,
    /// Cumulative bytes allocated into the old generation (bump + free-list
    /// reuse). The major-GC trigger keys off the *delta* of this counter:
    /// reused free space is churn too, so a monotonic `used_old` would
    /// under-count it.
    old_alloc: usize,
}

impl ArenaHeap {
    pub fn new(chunk_size: usize) -> Self {
        Self {
            young: ChunkedArena::new(chunk_size),
            old: ChunkedArena::new(chunk_size),
            old_alloc: 0,
        }
    }

    /// Bump-allocate a stable box holding `value`, tagged with `kind` for the
    /// sweep. The box's address never moves until the heap drops.
    #[inline]
    pub fn alloc_box_kind<T>(&mut self, kind: u64, value: T) -> *mut T {
        let p = self.young.alloc_region(std::mem::size_of::<T>(), kind);
        unsafe {
            (p as *mut T).write(value);
        }
        p as *mut T
    }

    /// Box with no droppable contents (opaque / `AString`).
    #[inline]
    pub fn alloc_box<T>(&mut self, value: T) -> *mut T {
        self.alloc_box_kind(KIND_RAW, value)
    }

    /// Bump-allocate a copy of `data`'s bytes in the young generation.
    #[inline]
    pub fn alloc_bytes(&mut self, data: &[u8]) -> *mut u8 {
        let p = self.alloc_bytes_uninit(data.len());
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        }
        p
    }

    /// Bump-allocate `size` uninitialized bytes in the young generation.
    #[inline]
    pub fn alloc_bytes_uninit(&mut self, size: usize) -> *mut u8 {
        self.young.alloc_region(size, KIND_RAW)
    }

    /// Bump-allocate `size` uninitialized raw bytes tagged `kind` — for
    /// boxes with an external layout (rope cons nodes, string builders)
    /// whose fields are written immediately by the caller.
    #[inline]
    pub fn alloc_raw_region(&mut self, size: usize, kind: u64) -> *mut u8 {
        self.young.alloc_region(size, kind)
    }

    /// Bump-allocate `size` uninitialized bytes in the old generation
    /// (reusing swept free space first) — used to flatten a promoted rope in
    /// place so the flat bytes live in the same generation as the box.
    #[inline]
    pub fn alloc_old_bytes_uninit(&mut self, size: usize) -> *mut u8 {
        self.old_alloc += (size + 7) & !7;
        self.old.alloc_region(size, KIND_RAW)
    }

    /// Is `addr` inside the young generation (i.e. an un-promoted value)?
    #[inline]
    pub fn addr_in_young(&self, addr: usize) -> bool {
        self.young.contains(addr)
    }

    /// Is `addr` inside the old generation (i.e. a promoted value)?
    #[inline]
    pub fn addr_in_old(&self, addr: usize) -> bool {
        self.old.contains(addr)
    }

    /// Copy a box from any arena into the old generation (promotion), reusing
    /// swept free space first. Returns the new payload address.
    /// Returns None if `addr` is not a tracked young region (callers may handle gracefully).
    #[inline]
    pub fn try_promote_box(&mut self, addr: usize) -> Option<usize> {
        let (size, kind) = self.young.region_at(addr)?;
        self.old_alloc += (size + 7) & !7;
        Some(self.old.copy_box_from(addr, size, kind as u64))
    }

    #[inline]
    pub fn promote_box(&mut self, addr: usize) -> usize {
        match self.try_promote_box(addr) {
            Some(p) => p,
            None => {
                eprintln!("[alloy] promote_box: no region record for {:#x} — returning null sentinel", addr);
                0
            }
        }
    }

    /// Copy `len` bytes from `src` (typically young string bytes) into the old
    /// generation, reusing swept free space first.
    #[inline]
    pub fn promote_bytes(&mut self, src: *const u8, len: usize) -> *mut u8 {
        self.old_alloc += (len + 7) & !7;
        self.old.alloc_bytes_from(src, len)
    }

    /// Cumulative bytes allocated into the old generation.
    #[inline]
    pub fn old_alloc_total(&self) -> usize {
        self.old_alloc
    }

    /// Bytes currently reclaimable on the old generation's free list.
    #[inline]
    pub fn free_bytes(&self) -> usize {
        self.old.free_list_bytes()
    }

    /// Number of free regions on the old generation's free list.
    #[inline]
    pub fn free_list_len(&self) -> usize {
        self.old.free_list_len()
    }

    /// Mark-sweep the old generation: drop the contents of every dead box
    /// (via `on_dead`) and coalesce the dead space onto the free list. `live`
    /// is the set of every reachable old payload address (boxes + string
    /// bytes). Live boxes are never moved.
    pub fn sweep_old(&mut self, live: &HashSet<usize>, mut on_dead: impl FnMut(usize, u64)) {
        self.old
            .sweep_free_list(|addr| live.contains(&addr), |a, k| on_dead(a, k));
    }

    /// Walk the old generation and return `(payload_addr, kind)` of every box
    /// whose write-barrier bit is set, clearing the bits. The caller
    /// re-traces each returned box (promoting young values written into it,
    /// and marking it while a sweep is being prepared).
    pub fn collect_dirty_old_boxes(&mut self) -> Vec<(usize, u8)> {
        self.old.collect_dirty_boxes()
    }

    /// Write barrier for the incremental GC: mark the box's slot dirty in
    /// the old generation's bitmap (young boxes are skipped — they are swept
    /// wholesale at the boundary anyway). Works through `&self`.
    #[inline]
    pub fn note_box_dirty(&self, addr: usize) {
        self.old.note_box_dirty(addr);
    }

    /// `KIND_*` of the region at `addr` (old first, then young), for the
    /// mark's string-byte handling. 0 if `addr` is not a tracked region.
    #[inline]
    pub fn kind_of(&self, addr: usize) -> u8 {
        if let Some((_, k)) = self.old.region_at(addr) {
            return k;
        }
        if let Some((_, k)) = self.young.region_at(addr) {
            return k;
        }
        0
    }

    /// Walk every allocation in the young generation (boxes and raw bytes),
    /// calling `f(payload_addr, kind, payload_size)` in allocation order.
    pub fn for_each_young_box(&self, f: impl FnMut(usize, u64, usize)) {
        self.young.for_each_region(f);
    }

    /// Walk every allocation in the old generation (boxes and raw bytes),
    /// calling `f(payload_addr, kind, payload_size)` in allocation order.
    pub fn for_each_old_box(&self, f: impl FnMut(usize, u64, usize)) {
        self.old.for_each_region(f);
    }

    /// Bulk-reset the young generation: everything not promoted is reclaimed
    /// with one cursor reset (callers must have dropped the contents of dead
    /// boxes first).
    pub fn reset_young(&mut self) {
        self.young.reset();
    }

    /// Bytes currently occupied in each generation.
    pub fn used_young(&self) -> usize {
        self.young.used()
    }

    pub fn used_old(&self) -> usize {
        self.old.used()
    }

    pub fn used(&self) -> usize {
        self.used_young() + self.used_old()
    }

    pub fn capacity(&self) -> usize {
        self.young.capacity() + self.old.capacity()
    }

    /// Number of chunks backing the young generation (debug/measurement).
    pub fn young_chunk_count(&self) -> usize {
        self.young.chunk_count()
    }

    /// Total bytes of the young generation's dirty bitmap (debug/measurement).
    pub fn young_dirty_bytes(&self) -> usize {
        self.young.dirty_bytes()
    }
}

impl Drop for ArenaHeap {
    /// Teardown: drop every live box's contents (element `Vec`s, shape `Rc`s,
    /// property values) before the chunks are bulk-freed, so Rc-backed
    /// payloads — natives capturing `Arc<SidecarMemory>`, closure cells,
    /// promise/channel handles — release exactly once. Without this the
    /// arena's bulk free leaked every non-trivially-droppable box (the
    /// `memory` module's natives kept the shared-memory segment file alive
    /// past a clean VM drop).
    ///
    /// Safety: the owner is quiescent here. Each region's contents are
    /// dropped exactly once: swept old regions are `KIND_FREE` (their
    /// contents were dropped by the sweep that freed them) and skipped, and
    /// every boundary sweep empties the young generation, so no young box at
    /// teardown shares ownership with a promoted old copy.
    fn drop(&mut self) {
        self.for_each_old_box(|addr, kind, _| crate::value::drop_box_contents(addr, kind));
        self.for_each_young_box(|addr, kind, _| crate::value::drop_box_contents(addr, kind));
    }
}

/// A per-unit escape-analysis result: young-address → promoted-address for
/// every box that survived the unit.
pub type PromoteMap = HashMap<usize, usize>;

thread_local! {
    /// The heap currently being allocated from, or null outside any context.
    static CURRENT_HEAP: Cell<*mut ArenaHeap> = const { Cell::new(std::ptr::null_mut()) };
    /// Fallback heap for allocations outside any context (module seeding,
    /// standalone tests). Lives for the thread's lifetime.
    static THREAD_HEAP: RefCell<ArenaHeap> = RefCell::new(ArenaHeap::new(1 << 20));
}

/// The heap allocations should go to right now. Never null: falls back to the
/// thread heap.
#[inline]
pub fn current_heap() -> *mut ArenaHeap {
    let p = CURRENT_HEAP.with(|c| c.get());
    if p.is_null() {
        THREAD_HEAP.with(|h| h.as_ptr() as *mut ArenaHeap)
    } else {
        p
    }
}

/// Installs `heap` as the active allocation context for its lifetime,
/// restoring the previous one on drop. The heap's owner must not free it
/// before the guard drops.
pub struct HeapGuard {
    prev: *mut ArenaHeap,
}

impl HeapGuard {
    pub fn set(heap: *mut ArenaHeap) -> Self {
        let prev = CURRENT_HEAP.with(|c| c.replace(heap));
        Self { prev }
    }
}

impl Drop for HeapGuard {
    fn drop(&mut self) {
        CURRENT_HEAP.with(|c| c.set(self.prev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heap_boxes_and_bytes_roundtrip() {
        let mut heap = ArenaHeap::new(256);
        let p = heap.alloc_box(42u64);
        assert_eq!(unsafe { *p }, 42);
        let bytes = heap.alloc_bytes(b"hello arena");
        unsafe {
            assert_eq!(std::slice::from_raw_parts(bytes, 11), b"hello arena");
        }
    }

    #[test]
    fn test_heap_guard_restores_previous() {
        let mut a = ArenaHeap::new(64);
        let mut b = ArenaHeap::new(64);
        let g1 = HeapGuard::set(&mut a);
        assert!(std::ptr::eq(current_heap(), &mut a));
        let g2 = HeapGuard::set(&mut b);
        assert!(std::ptr::eq(current_heap(), &mut b));
        drop(g2);
        assert!(std::ptr::eq(current_heap(), &mut a));
        drop(g1);
        // Outside any guard: the thread fallback heap.
        assert!(std::ptr::eq(current_heap(), THREAD_HEAP.with(|h| h.as_ptr() as *mut ArenaHeap)));
    }

    #[test]
    fn test_heap_grows_past_initial_chunk() {
        let mut heap = ArenaHeap::new(64);
        let mut ptrs = Vec::new();
        for i in 0..10_000u64 {
            ptrs.push(heap.alloc_box(i));
        }
        for (idx, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { **p }, idx as u64);
        }
        assert!(heap.capacity() > 64);
    }
}
