use std::alloc::{Layout, alloc, dealloc};
use std::ptr::NonNull;
use std::cell::Cell;


#[derive(Debug)]
pub struct Arena {
    ptr: NonNull<u8>,
    layout: Layout,
    /// Bump offset. `Cell` (not `AtomicUsize`) because arenas are single-owner:
    /// allocation is only safe from one thread at a time. `Cell` is `!Sync`,
    /// so the compiler prevents concurrent `&Arena` access at the type level.
    /// The `SidecarMemory` segment (which IS shared across threads/processes)
    /// uses its own `AtomicUsize` for the write cursor — see `shared_memory.rs`.
    offset: Cell<usize>,
    capacity: usize,
}

// SAFETY: Arena is Send so ownership can transfer between threads (e.g. a VM
// moving to a worker thread). It is intentionally NOT Sync — Cell<usize> is
// !Sync, enforcing at the type level that concurrent &Arena access is a
// compile error. All allocation methods take &self (single-threaded bump).
unsafe impl Send for Arena {}

impl Arena {
    pub fn new(capacity: usize) -> Self {
        let layout = Layout::from_size_align(capacity, 16).expect("invalid arena layout");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            layout,
            offset: Cell::new(0),
            capacity,
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();

        let current = self.offset.get();
        let aligned = (current + align - 1) & !(align - 1);

        if aligned + size > self.capacity {
            std::alloc::handle_alloc_error(
                Layout::from_size_align(size, align).unwrap()
            );
        }

        let ptr = unsafe { self.ptr.as_ptr().add(aligned) as *mut T };
        self.offset.set(aligned + size);
        unsafe {
            ptr.write(value);
            &mut *ptr
        }
    }

    pub fn alloc_bytes(&self, data: &[u8]) -> *mut u8 {
        let size = data.len();
        let align = 8;
        let current = self.offset.get();
        let aligned = (current + align - 1) & !(align - 1);

        if aligned + size > self.capacity {
            std::alloc::handle_alloc_error(
                Layout::from_size_align(size, align).unwrap()
            );
        }

        let ptr = unsafe { self.ptr.as_ptr().add(aligned) };
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, size);
            self.offset.set(aligned + size);
            ptr
        }
    }

    pub fn bump_alloc(&self, size: usize, align: usize) -> *mut u8 {
        let current = self.offset.get();
        let aligned = (current + align - 1) & !(align - 1);

        if aligned + size > self.capacity {
            std::alloc::handle_alloc_error(
                Layout::from_size_align(size, align).unwrap()
            );
        }

        let ptr = unsafe { self.ptr.as_ptr().add(aligned) };
        self.offset.set(aligned + size);
        ptr
    }

    pub fn reset(&self) {
        self.offset.set(0);
    }

    /// Advance the bump cursor by `n` bytes without writing anything.
    pub fn advance(&self, n: usize) {
        let current = self.offset.get();
        assert!(current + n <= self.capacity, "arena bump past capacity");
        self.offset.set(current + n);
    }

    pub fn used(&self) -> usize {
        self.offset.get()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

/// Kind reserved for swept (dead) regions on the old generation's free list.
/// Kinds 0-3 are used by the value heap (raw/string/array/object); 15 is
/// never produced by the value heap, so it can't collide.
pub const KIND_FREE: u64 = 15;

/// One tracked region in a chunk: the payload lives at `base + slot * 8`
/// (8-aligned) and occupies `round8(size)` bytes. The region table replaces
/// the old inline 8-byte header entirely — payloads are packed with zero
/// padding between them, the linear walk is a Vec iteration instead of
/// pointer-chasing, and swept space never shares the payload address space
/// (free-list bookkeeping lives only in the bins + table, not in memory).
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Slot index within the chunk: `addr = chunk.base + slot * 8`.
    pub slot: u32,
    /// Payload size in bytes (the region's span is `round8(size)`).
    pub size: u32,
    /// `KIND_*` (15 = swept/free).
    pub kind: u8,
}

impl Region {
    /// Payload address for a chunk base.
    #[inline]
    pub fn addr(&self, base: usize) -> usize {
        base + (self.slot as usize) * 8
    }

    /// Physical span in bytes (payload rounded up to the 8-byte slot grid).
    #[inline]
    pub fn span(&self) -> usize {
        (self.size as usize + 7) & !7
    }
}

/// A growable bump arena: allocation pointer-bumps inside fixed-size chunks
/// (a new chunk is appended when the active one fills, so addresses handed
/// out earlier stay valid for the arena's lifetime), and `reset` reclaims
/// everything with O(chunks) work and **no per-object free** — the "GC
/// killer" pattern behind the PRD's arena-allocator pillar. Callers that
/// store only trivially-droppable payloads (like the VM's microtask records)
/// get constant-time allocation and constant-time teardown.
///
/// The old generation additionally supports **mark-sweep reclamation**:
/// swept (dead) regions are coalesced into runs (their headers become
/// `KIND_FREE`, so the linear walker stays in sync), binned by size class,
/// and later allocations reuse that space before bumping — **LIFO per size
/// class**, so a churned same-size object is reallocated at the same
/// address (temporal locality). Live boxes never move — a non-copying GC —
/// so a huge live set costs a mark + sweep, not a full copy.
#[derive(Debug)]
pub struct ChunkedArena {
    chunks: Vec<Arena>,
    active: usize,
    chunk_size: usize,
    /// Reclaimed regions from mark-sweep: `(payload_addr, payload_size)`
    /// (8-aligned payload sizes), grouped into **size classes** — bin `i`
    /// holds regions whose size maps to class `i` via [`class_idx`]. The
    /// sweep rebuilds the bins wholesale from the region-table walk;
    /// allocation pops LIFO from its class (scanning larger classes when the
    /// exact one is empty) and splits + re-bins remainders. The bins are the
    /// *only* bookkeeping for free space — nothing is written into the
    /// payload address space.
    free_bins: Vec<Vec<(usize, usize)>>,
    /// Per-chunk region tables, parallel to `chunks` (chunks that only use
    /// the untracked `alloc_at` path — the VM's microtask arena — keep an
    /// empty table). Sorted by slot; a contiguous partition of the chunk's
    /// used space (bump appends, reuse updates in place, splits splice).
    regions: Vec<Vec<Region>>,
    /// Per-chunk write-barrier bitmap, parallel to `chunks`: one bit per
    /// 8-byte slot (`slot >> 6` indexes the `u64`), set by the incremental
    /// GC barrier on every box write. `Cell` so the barrier can mark through
    /// `&self`. Sized to the chunk's capacity at chunk creation.
    dirty: Vec<Vec<Cell<u64>>>,
}

/// Size-class mapping for the segregated free list. Classes grow denser at
/// small sizes (where most boxes live: 16-byte `AString`, 32-byte array box,
/// 64-byte object box — all exact) and sparser for huge string bytes, where
/// internal padding is a bounded fraction of the allocation.
///
/// Class `0..=7`   : 8..=64    step 8   (exact for every box type)
/// Class `8..=19`  : 80..=256   step 16
/// Class `20..=31` : 320..=1024 step 64
/// Class `32..=43` : 1280..=4096 step 256
/// Class `44..=55` : 5120..=16384 step 1024
/// Class `56+`     : 20480.. step 8192
#[inline]
pub fn class_idx(size: usize) -> usize {
    if size <= 64 {
        size.div_ceil(8) - 1
    } else if size <= 256 {
        8 + (size - 64).div_ceil(16) - 1
    } else if size <= 1024 {
        20 + (size - 256).div_ceil(64) - 1
    } else if size <= 4096 {
        32 + (size - 1024).div_ceil(256) - 1
    } else if size <= 16384 {
        44 + (size - 4096).div_ceil(1024) - 1
    } else {
        56 + (size - 16384).div_ceil(8192) - 1
    }
}

impl ChunkedArena {
    pub fn new(chunk_size: usize) -> Self {
        let mut s = Self {
            chunks: Vec::new(),
            active: 0,
            chunk_size,
            free_bins: Vec::new(),
            regions: Vec::new(),
            dirty: Vec::new(),
        };
        s.push_chunk(chunk_size);
        s
    }

    /// Append a fresh chunk of at least `cap` bytes plus its empty region
    /// table and a dirty bitmap sized to its capacity.
    fn push_chunk(&mut self, cap: usize) {
        let cap = cap.max(self.chunk_size);
        self.chunks.push(Arena::new(cap));
        self.regions.push(Vec::new());
        // One bit per 8-byte slot: (cap >> 3) slots, 64 per u64 cell.
        self.dirty.push(vec![Cell::new(0u64); (cap >> 3).div_ceil(64)]);
    }

    /// Bump-allocate `value`, returning a pointer that stays valid until the
    /// arena is dropped. Never aborts: on overflow a fresh chunk is appended
    /// and the existing addresses are untouched.
    pub fn alloc_at<T>(&mut self, value: T) -> *mut T {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        loop {
            let chunk = &self.chunks[self.active];
            let current = chunk.used();
            let aligned = (current + align - 1) & !(align - 1);
            if aligned + size <= chunk.capacity() {
                let ptr = unsafe { chunk.ptr().add(aligned) as *mut T };
                chunk.advance(aligned + size - current);
                unsafe {
                    ptr.write(value);
                }
                return ptr;
            }
            // Grow: next chunk at least doubles the active capacity so the
            // amortized cost of chunk allocation stays O(1) per record.
            let cap = self
                .chunk_size
                .max(size + align)
                .max(self.chunks[self.active].capacity().saturating_mul(2));
            self.push_chunk(cap);
            self.active += 1;
        }
    }

    /// Bitwise-move a payload out of the arena (the slot stays resident until
    /// `reset`/drop). **Unsafe contract**: the caller must never re-read or
    /// drop the slot afterwards — the slot is bulk-reclaimed, not dropped, so
    /// ownership of the moved-out value transfers to the caller exactly once.
    pub unsafe fn read_at<T>(&self, ptr: *const T) -> T {
        std::ptr::read(ptr)
    }

    /// Bump-allocate a copy of `data`, returning a byte pointer that stays
    /// valid until the arena is dropped. Grows a new chunk on overflow like
    /// `alloc_at`.
    pub fn alloc_bytes(&mut self, data: &[u8]) -> *mut u8 {
        let size = data.len();
        let ptr = self.alloc_bytes_uninit(size);
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, size);
        }
        ptr
    }

    /// Bump-allocate `size` uninitialized bytes (the caller fills them),
    /// recording a `KIND_RAW` region. The physical cursor advances by
    /// `round8(size)` and the payload is 8-aligned, so payloads are packed
    /// with **zero padding** between them (no inline header).
    pub fn alloc_bytes_uninit(&mut self, size: usize) -> *mut u8 {
        self.alloc_region(size, 0)
    }

    /// Bump-allocate `size` uninitialized bytes as a region of `kind`,
    /// returning the payload pointer. No header is written into memory — the
    /// `(slot, size, kind)` record lives only in this chunk's region table.
    /// Every region occupies at least one 8-byte slot (a 0-length request
    /// like an empty string's bytes still gets a stable 8-byte span), so the
    /// table stays a strictly-slot-partitioning walk.
    pub fn alloc_region(&mut self, size: usize, kind: u64) -> *mut u8 {
        let rsize = ((size + 7) & !7).max(8);
        loop {
            let chunk = &self.chunks[self.active];
            let current = chunk.used();
            if current + rsize <= chunk.capacity() {
                let slot = (current >> 3) as u32;
                let ptr = unsafe { chunk.ptr().add(current) };
                chunk.advance(rsize);
                self.regions[self.active].push(Region {
                    slot,
                    size: size as u32,
                    kind: kind as u8,
                });
                return ptr;
            }
            // Overflow: move to the next already-allocated chunk first (a
            // reset zeroes every cursor, so the later chunks are reusable
            // even though `active` points at the first) — only grow when the
            // arena has no more chunks left. Without this, a per-request
            // reset + append pattern would pile up chunks forever.
            if self.active + 1 < self.chunks.len() {
                self.active += 1;
                continue;
            }
            let cap = self
                .chunk_size
                .max(rsize)
                .max(self.chunks[self.active].capacity().saturating_mul(2));
            self.push_chunk(cap);
            self.active += 1;
        }
    }

    /// Reuse swept space first: pop the most-recently-freed region of the
    /// request's size class (LIFO → temporal locality: a churned same-size
    /// object is reallocated at the same address), scanning larger classes
    /// when the exact one is empty. Bumping only when nothing fits. Returns
    /// the payload pointer, or `None` when no free region is large enough.
    /// On a split, the remainder keeps its `KIND_FREE` record (the table is
    /// spliced: child + remainder replace the parent); when the remainder is
    /// too small to host a region (payload < 8), the box is padded to
    /// consume the whole span (the caller copies only what it needs). No
    /// header bytes are ever written into the payload address space.
    pub fn alloc_free(&mut self, size: usize, kind: u64) -> Option<*mut u8> {
        // Min 8 like the bump path, so a 0-length request (empty string
        // bytes) never underflows the size-class mapping.
        let rsize = ((size + 7) & !7).max(8);
        let mut ci = class_idx(rsize);
        while ci < self.free_bins.len() {
            match self.free_bins[ci].pop() {
                None => ci += 1,
                Some((addr, p)) => {
                    if p < rsize {
                        // Mis-binned or in a sparser class: put it back where
                        // it belongs and keep scanning larger classes.
                        self.free_bins[class_idx(p)].push((addr, p));
                        ci += 1;
                        continue;
                    }
                    // Locate the parent's region record (it is KIND_FREE).
                    let (chunk_i, slot) = self.chunk_of(addr);
                    let rem = p - rsize;
                    let regions = &mut self.regions[chunk_i];
                    let idx = regions.partition_point(|r| r.slot < slot as u32);
                    debug_assert!(idx < regions.len() && regions[idx].slot == slot as u32);
                    let (child_size, rem_slot, rem_size) = if rem >= 8 {
                        // Split: child at the parent's slot, remainder after.
                        (size, slot as u32 + (rsize >> 3) as u32, rem)
                    } else if rem > 0 {
                        // Too small to host a region: pad to fill the span.
                        (p, 0, 0)
                    } else {
                        (size, 0, 0)
                    };
                    if rem_size > 0 {
                        let rem_addr = addr + rsize;
                        self.free_bins[class_idx(rem_size)].push((rem_addr, rem_size));
                        regions[idx] = Region { slot: slot as u32, size: child_size as u32, kind: kind as u8 };
                        regions.insert(
                            idx + 1,
                            Region { slot: rem_slot, size: rem_size as u32, kind: KIND_FREE as u8 },
                        );
                    } else {
                        regions[idx] = Region { slot: slot as u32, size: child_size as u32, kind: kind as u8 };
                    }
                    return Some(addr as *mut u8);
                }
            }
        }
        None
    }

    /// Find the chunk index and slot of a payload address. Returns `None` if
    /// the address does not belong to any active chunk — callers MUST handle
    /// the `None` case instead of acting on a stale/bogus address.
    fn try_chunk_of(&self, addr: usize) -> Option<(usize, usize)> {
        for (i, c) in self.chunks.iter().take(self.active + 1).enumerate() {
            let base = c.ptr() as usize;
            if addr >= base && addr < base + c.used() {
                return Some((i, (addr - base) >> 3));
            }
        }
        None
    }
    fn chunk_of(&self, addr: usize) -> (usize, usize) {
        match self.try_chunk_of(addr) {
            Some(v) => v,
            None => {
                // Debug builds: loud crash so the bug is caught immediately.
                debug_assert!(false, "[alloy] address {:#x} not in any arena chunk — GC would corrupt chunk 0", addr);
                // Release builds: log and return a sentinel that sweep callers
                // must check. Using (usize::MAX, 0) so no real chunk index
                // can match — callers that destructure blindly will
                // bounds-check fail rather than silently corrupt.
                eprintln!("[alloy] address not in arena: {:#x}", addr);
                (usize::MAX, 0)
            }
        }
    }

    /// Mark-sweep reclamation: walk the arena linearly, call `on_dead(addr,
    /// kind)` for every dead (un-marked) region's payload, coalesce all dead
    /// regions (live or previously-free) into runs, rewrite the run headers
    /// as `KIND_FREE`, and rebuild the free list from the runs. Live boxes are
    /// untouched — never copied, never moved. `is_live(payload_addr)` must
    /// return true exactly for the regions reachable from the roots (marking
    /// string bytes is the caller's job).
    /// Mark-sweep reclamation: walk the region tables (the linear walk is a
    /// Vec iteration now — no header reads), call `on_dead(addr, kind)` for
    /// every dead (un-marked) region's payload, coalesce all dead regions
    /// (live or previously-free) into runs, rewrite their table records as
    /// `KIND_FREE`, and rebuild the free bins from the runs. Live boxes are
    /// untouched — never copied, never moved. `is_live(payload_addr)` must
    /// return true exactly for the regions reachable from the roots (marking
    /// string bytes is the caller's job). No bytes are written into the
    /// payload address space — the free-list bookkeeping lives only in the
    /// table and bins.
    pub fn sweep_free_list(
        &mut self,
        is_live: impl Fn(usize) -> bool,
        mut on_dead: impl FnMut(usize, u64),
    ) {
        let mut new_bins: Vec<Vec<(usize, usize)>> = Vec::new();
        for ci in 0..self.chunks.len().min(self.active + 1) {
            let base = self.chunks[ci].ptr() as usize;
            let old = std::mem::take(&mut self.regions[ci]);
            let mut new_regions: Vec<Region> = Vec::with_capacity(old.len());
            // Current free run: (slot of the first dead region, span sum).
            let mut run: Option<(u32, usize)> = None;
            for r in old {
                let addr = r.addr(base);
                if r.kind == KIND_FREE as u8 || !is_live(addr) {
                    if r.kind != KIND_FREE as u8 {
                        on_dead(addr, r.kind as u64);
                    }
                    match &mut run {
                        Some((_, p)) => *p += r.span(),
                        None => run = Some((r.slot, r.span())),
                    }
                } else {
                    if let Some((s, p)) = run.take() {
                        new_regions.push(Region { slot: s, size: p as u32, kind: KIND_FREE as u8 });
                        let ci_bin = class_idx(p);
                        if new_bins.len() <= ci_bin {
                            new_bins.resize(ci_bin + 1, Vec::new());
                        }
                        new_bins[ci_bin].push((base + (s as usize) * 8, p));
                    }
                    new_regions.push(r);
                }
            }
            if let Some((s, p)) = run.take() {
                new_regions.push(Region { slot: s, size: p as u32, kind: KIND_FREE as u8 });
                let ci_bin = class_idx(p);
                if new_bins.len() <= ci_bin {
                    new_bins.resize(ci_bin + 1, Vec::new());
                }
                new_bins[ci_bin].push((base + (s as usize) * 8, p));
            }
            self.regions[ci] = new_regions;
        }
        self.free_bins = new_bins;
    }

    /// Total bytes currently reclaimable on the free list (region spans +
    /// the 8-byte slot grid they occupy).
    pub fn free_list_bytes(&self) -> usize {
        self.free_bins.iter().flatten().map(|&(_, p)| p).sum()
    }

    pub fn free_list_len(&self) -> usize {
        self.free_bins.iter().map(|b| b.len()).sum()
    }

    /// Copy `size` payload bytes from `payload_addr` (which may point into
    /// any arena) into this one as a region of `kind`. Returns the new
    /// payload address. The source box is untouched — the caller is
    /// responsible for fixing up the copy's interior references (and, for
    /// arena-backed strings, copying the bytes, which live outside the box).
    /// Reuses swept space first.
    pub fn copy_box_from(&mut self, payload_addr: usize, size: usize, kind: u64) -> usize {
        let new_base = match self.alloc_free(size, kind) {
            Some(p) => p,
            None => self.alloc_region(size, kind),
        };
        unsafe {
            std::ptr::copy_nonoverlapping(payload_addr as *const u8, new_base, size);
        }
        new_base as usize
    }

    /// Bump-allocate `len` raw bytes in this arena (KIND_RAW region) and copy
    /// them from `src`, which may point into any other arena. Reuses swept
    /// space first.
    ///
    /// # Safety
    /// `src` must be a valid pointer to at least `len` readable bytes.
    pub unsafe fn alloc_bytes_from(&mut self, src: *const u8, len: usize) -> *mut u8 {
        let p = match self.alloc_free(len, 0) {
            Some(p) => p,
            None => self.alloc_region(len, 0),
        };
        unsafe {
            std::ptr::copy_nonoverlapping(src, p, len);
        }
        p
    }

    /// Reclaim every chunk in O(chunks) time: one cursor reset, no per-record
    /// drops. Safe only when no live payload remains in the arena.
    pub fn reset(&mut self) {
        // Capture each chunk's used bytes before zeroing the cursors, so the
        // dirty-bitmap clear below only touches cells that could be set.
        let used_cells: Vec<usize> = self
            .chunks
            .iter()
            .map(|c| (c.used() >> 3 >> 6) + 1)
            .collect();
        for c in &self.chunks {
            c.reset();
        }
        self.active = 0;
        self.free_bins.clear();
        for r in &mut self.regions {
            r.clear();
        }
        // Clear only the bitmap cells covering bytes actually used this cycle
        // — O(used), not O(capacity).
        for (d, &cells) in self.dirty.iter().zip(used_cells.iter()) {
            for cell in d.iter().take(cells.min(d.len())) {
                cell.set(0);
            }
        }
    }

    /// Bytes currently occupied (across chunks, up to the active one).
    pub fn used(&self) -> usize {
        self.chunks.iter().take(self.active + 1).map(|c| c.used()).sum()
    }

    /// Does `addr` fall inside an allocated region of any chunk? (Used by the
    /// escape-analysis pass to tell young-arena values from promoted ones.)
    pub fn contains(&self, addr: usize) -> bool {
        self.chunks
            .iter()
            .take(self.active + 1)
            .any(|c| {
                let base = c.ptr() as usize;
                addr >= base && addr < base + c.used()
            })
    }

    /// `(size, kind)` of the region whose payload is at `addr`, or `None`.
    /// Binary search over the chunk's slot-sorted region table.
    pub fn region_at(&self, addr: usize) -> Option<(usize, u8)> {
        for (i, c) in self.chunks.iter().take(self.active + 1).enumerate() {
            let base = c.ptr() as usize;
            if addr >= base && addr < base + c.used() {
                let slot = ((addr - base) >> 3) as u32;
                let rs = &self.regions[i];
                let idx = rs.partition_point(|r| r.slot < slot);
                if idx < rs.len() && rs[idx].slot == slot {
                    return Some((rs[idx].size as usize, rs[idx].kind));
                }
                return None;
            }
        }
        None
    }

    /// Incremental-GC write barrier: set the dirty bit for the slot holding
    /// `addr`'s payload. Works through `&self` (`Cell`), so the VM can mark
    /// on its hottest path without a mutable borrow. Setting a bit for a
    /// region that is not a region start is harmless — the scan only reads
    /// region-start slots.
    pub fn note_box_dirty(&self, addr: usize) {
        for (i, c) in self.chunks.iter().take(self.active + 1).enumerate() {
            let base = c.ptr() as usize;
            if addr >= base && addr < base + c.used() {
                let slot = (addr - base) >> 3;
                let bm = &self.dirty[i];
                let cell = &bm[slot >> 6];
                cell.set(cell.get() | (1u64 << (slot & 63)));
                return;
            }
        }
    }

    /// Walk every tracked region in allocation order, calling
    /// `f(payload_addr, kind, payload_size)`. This is the linear walk the
    /// sweeps use — a Vec iteration, no header reads.
    pub fn for_each_region(&self, mut f: impl FnMut(usize, u64, usize)) {
        for (i, c) in self.chunks.iter().take(self.active + 1).enumerate() {
            let base = c.ptr() as usize;
            for r in &self.regions[i] {
                f(r.addr(base), r.kind as u64, r.size as usize);
            }
        }
    }

    /// Collect every old region whose write-barrier bit is set, clearing the
    /// bits, as `(payload_addr, kind)`. The bitmap indexes slots directly, so
    /// this is a Vec scan + one bit test per region.
    pub fn collect_dirty_boxes(&mut self) -> Vec<(usize, u8)> {
        let mut out = Vec::new();
        for (i, c) in self.chunks.iter().take(self.active + 1).enumerate() {
            let base = c.ptr() as usize;
            let bm = &self.dirty[i];
            for r in &self.regions[i] {
                let slot = r.slot as usize;
                if r.kind != KIND_FREE as u8 && (bm[slot >> 6].get() & (1u64 << (slot & 63))) != 0 {
                    bm[slot >> 6].set(bm[slot >> 6].get() & !(1u64 << (slot & 63)));
                    out.push((r.addr(base), r.kind));
                }
            }
        }
        out
    }

    pub fn capacity(&self) -> usize {
        self.chunks.iter().map(|c| c.capacity()).sum()
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Total bytes of the write-barrier bitmaps (debug/measurement).
    pub fn dirty_bytes(&self) -> usize {
        self.dirty.iter().map(|d| d.len() * 8).sum()
    }

    /// Sum of the region spans — must equal `used()` (the table is a
    /// contiguous partition of the chunk's used space; a mismatch means a
    /// bookkeeping bug). Exposed for tests and the density measurement.
    pub fn tracked_span(&self) -> usize {
        self.regions
            .iter()
            .take(self.active + 1)
            .map(|rs| rs.iter().map(|r| r.span()).sum::<usize>())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arena_alloc_primitives() {
        let arena = Arena::new(1024);
        let x = arena.alloc(42u64);
        assert_eq!(*x, 42);
        let y = arena.alloc(3.5f64);
        assert!((*y - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_arena_alloc_bytes() {
        let arena = Arena::new(1024);
        let data = b"hello world";
        let ptr = arena.alloc_bytes(data);
        let slice = unsafe { std::slice::from_raw_parts(ptr, data.len()) };
        assert_eq!(slice, data);
    }

    #[test]
    fn test_arena_reset() {
        let arena = Arena::new(1024);
        arena.alloc(1u32);
        arena.alloc(2u32);
        assert!(arena.used() > 0);
        arena.reset();
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn test_chunked_arena_roundtrip_and_growth() {
        let mut arena = ChunkedArena::new(64);
        let n = 10_000u64;
        let mut ptrs = Vec::new();
        for i in 0..n {
            ptrs.push(arena.alloc_at(i));
        }
        // Grows past the initial 64-byte chunk without invalidating earlier
        // addresses.
        assert!(arena.chunk_count() > 1);
        for (idx, p) in ptrs.iter().enumerate() {
            assert_eq!(unsafe { arena.read_at(*p) }, idx as u64);
        }
        let used_before = arena.used();
        assert!(used_before >= n as usize * 8);
        arena.reset();
        assert_eq!(arena.used(), 0);
        // Addresses are reusable after the bulk reset (the GC-killer pattern).
        let p = arena.alloc_at(42u64);
        assert_eq!(unsafe { arena.read_at(p) }, 42);
    }
}
