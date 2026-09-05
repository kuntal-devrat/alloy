use crate::heap::{self, KIND_RAW};
use crate::regex;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

// ---- NaN-boxing layout ------------------------------------------------------
//
// A Value is one 64-bit word. Real IEEE doubles occupy almost the whole space;
// we steal the *negative NaN* region (top 16 bits 0xFFF8..0xFFFF) for tagged
// values. `Value::number` canonicalizes any negative NaN it is given, so real
// doubles never collide with the tags.
//
//   top16 == FFF8   Int:      bits 0-47 are the i48 payload (sign-extended)
//   top16 == FFF9   Small:    low 4 bits: 0=Undefined 1=Null 2=False 3=True
//                             4=Symbol (bits 4-47 = id)
//   top16 == FFFA   Array:    bits 0-47 = *const RefCell<Vec<Value>> (arena box)
//   top16 == FFFB   Object:   bits 0-47 = *const RefCell<ObjectData> (arena box)
//   top16 == FFFC   String:   bits 0-47 = *const AString (arena box: bytes+len)
//   top16 == FFFD   Cell:     bits 0-47 = *const RefCell<Value> (Rc inner)
//   top16 == FFFE   Function: bits 0-47 = *const FunctionData
//   top16 == FFFF   Misc:     bits 0-47 = *const MiscBox (Promise/Native/Pointer/
//                             Buffer/BigInt)
//
// String/array/object payloads point into the active [`heap::ArenaHeap`]: the
// boxes are bump-allocated and bulk-freed at teardown, so clones are plain bit
// copies and drops are no-ops — no reference-count traffic in the hot path.
// Cells, functions, and misc values keep `Rc` (they carry mutable shared state
// or host handles that outlive the value heap).

const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const TAG_INT: u64 = 0xFFF8_0000_0000_0000;
const TAG_SMALL: u64 = 0xFFF9_0000_0000_0000;
const TAG_ARR: u64 = 0xFFFA_0000_0000_0000;
const TAG_OBJ: u64 = 0xFFFB_0000_0000_0000;
const TAG_STR: u64 = 0xFFFC_0000_0000_0000;
const TAG_CELL: u64 = 0xFFFD_0000_0000_0000;
const TAG_FN: u64 = 0xFFFE_0000_0000_0000;
const TAG_MISC: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

const S_UNDEF: u64 = 0;
const S_NULL: u64 = 1;
const S_FALSE: u64 = 2;
const S_TRUE: u64 = 3;
const S_SYMBOL: u64 = 4;

/// Canonical quiet NaN (a positive NaN, safe from the tag space).
const CANON_NAN: u64 = 0x7FF8_0000_0000_0000;

/// Sign-extend a 48-bit payload back to a full pointer/usize.
#[inline(always)]
fn payload_to_usize(w: u64) -> usize {
    ((w << 16) as i64 >> 16) as usize
}

#[inline(always)]
fn usize_to_payload(p: usize) -> u64 {
    (p as u64) & PAYLOAD_MASK
}

/// Convert an `Rc`'s inner pointer to a payload, transferring ownership of the
/// Rc's single strong reference into the Value (the Rc is leaked on purpose;
/// the Value's drop releases it).
#[inline]
fn own_rc<T>(rc: Rc<T>) -> u64 {
    let p = usize_to_payload(Rc::as_ptr(&rc) as usize);
    std::mem::forget(rc);
    p
}

/// Arena-backed immutable string, either **flat** (a bytes pointer + length,
/// the classic form) or a **rope** (ConsString-style: two child string
/// payloads packed into the same 16-byte box, no byte copies). Ropes are
/// built O(1) by [`Value::rope`] for the hot `s = s + t` path and flattened
/// lazily — once, into the box's own generation — the first time the string
/// is actually read (print, index, compare, length). Stable address; clones
/// of the containing `Value` are bit copies.
///
/// The three forms share one `#[repr(C)]` layout and are told apart by the
/// top 16 bits of the `bytes` word: flat payloads are real arena pointers
/// (top 16 bits zero), a cons node stores its left child's full `Value` bits
/// there (tag `TAG_STR`, 0xFFFC), and a builder stores the tag `TAG_OBJ`
/// (0xFFFB) — its real payload lives in the box's extra words.
///
/// Box layouts (regions are 16 or 32 bytes, recorded in the side table):
///
/// * **flat** (16B): `{ bytes, len }`
/// * **cons**  (32B): `{ left, right, cached_total_len }`
/// * **builder** (32B): `{ TAG_OBJ marker, len, bytes, cap }` — a growable
///   flat buffer: `s = s + leaf` loops append the leaf into the buffer
///   (never overwriting written bytes, so aliased readers of older boxes
///   stay valid) and allocate a fresh box per append (strings are immutable
///   — old boxes must keep their prefix). Doubling growth makes appends
///   amortized O(1), and the final string is already contiguous: reading it
///   needs no flatten.
#[repr(C)]
pub struct AString {
    bytes: *mut u8,
    len: usize,
}

impl AString {
    /// Is this box a cons (rope) node rather than a flat string?
    #[inline]
    pub fn is_cons(&self) -> bool {
        (self.bytes as u64) >> 48 == (TAG_STR >> 48)
    }

    /// Is this box a growable flat-buffer builder?
    #[inline]
    pub fn is_builder(&self) -> bool {
        (self.bytes as u64) >> 48 == (TAG_OBJ >> 48)
    }

    /// The left child `Value` of a cons node.
    #[inline]
    pub fn left(&self) -> Value {
        debug_assert!(self.is_cons());
        Value(self.bytes as u64)
    }

    /// The right child `Value` of a cons node.
    #[inline]
    pub fn right(&self) -> Value {
        debug_assert!(self.is_cons());
        Value(self.len as u64)
    }

    /// Bytes used so far by a builder box.
    #[inline]
    pub fn builder_len(&self) -> usize {
        debug_assert!(self.is_builder());
        unsafe { *((self as *const AString as *const u8).add(8) as *const usize) }
    }

    /// The growable buffer of a builder box.
    #[inline]
    pub fn builder_bytes(&self) -> *mut u8 {
        debug_assert!(self.is_builder());
        unsafe { *((self as *const AString as *const u8).add(16) as *const usize) as *mut u8 }
    }

    /// Capacity of a builder box's buffer.
    #[inline]
    pub fn builder_cap(&self) -> usize {
        debug_assert!(self.is_builder());
        unsafe { *((self as *const AString as *const u8).add(24) as *const usize) }
    }

    /// Repoint a builder box's buffer (used by promotion).
    #[inline]
    pub fn set_builder_bytes(&mut self, p: *mut u8) {
        debug_assert!(self.is_builder());
        unsafe { *((self as *mut AString as *mut u8).add(16) as *mut usize) = p as usize; }
    }

    /// The contiguous byte payload for flat and builder forms (cons boxes
    /// must be flattened first).
    #[inline]
    pub fn contiguous_bytes(&self) -> *mut u8 {
        debug_assert!(!self.is_cons());
        if self.is_builder() {
            self.builder_bytes()
        } else {
            self.bytes
        }
    }

    /// The string's byte payload (arena-backed; valid while the heap lives).
    #[inline]
    pub fn bytes_ptr(&self) -> *mut u8 {
        debug_assert!(!self.is_cons());
        self.contiguous_bytes()
    }

    /// The string's length in bytes. O(1) in all three forms: cons boxes
    /// cache their total after the two child slots, builders carry the
    /// bytes used so far — repeated `.length` on a growing string never
    /// re-walks a tree.
    #[inline]
    pub fn len(&self) -> usize {
        if self.is_cons() {
            unsafe { *((self as *const AString as *const u8).add(16) as *const usize) }
        } else if self.is_builder() {
            self.builder_len()
        } else {
            self.len
        }
    }

    /// The byte payload as a slice (flattens a rope in place first).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        if self.is_cons() {
            flatten_rope_in_place(self as *const AString as *mut AString);
        }
        let b = self.contiguous_bytes();
        let n = self.len();
        unsafe { std::slice::from_raw_parts(b, n) }
    }
}

/// Flatten a rope (cons) box in place: concatenate the leaves into a fresh
/// buffer **in the box's own generation** (old for promoted boxes, so the
/// invariant that a box and its bytes share a generation holds) and rewrite
/// the box to flat. Called on first read; later reads are O(1). Iterative,
/// so arbitrarily deep left-leaning ropes can't overflow the stack.
/// Copy a string subtree's bytes in-order (left leaves, right leaves) into
/// `buf` starting at `pos`, returning the new position. Iterative, so deep
/// left-leaning ropes built by `s += t` can't overflow the stack.
fn fill_string_into(buf: *mut u8, mut pos: usize, root: usize) -> usize {
    let mut stack: Vec<(bool, usize)> = vec![(false, root)];
    while let Some((left_done, a)) = stack.pop() {
        let bb = unsafe { &*(a as *const AString) };
        if bb.is_cons() {
            if !left_done {
                let left = bb.left();
                stack.push((true, a));
                stack.push((false, payload_to_usize(left.0)));
            } else {
                let right = bb.right();
                stack.push((false, payload_to_usize(right.0)));
            }
        } else if bb.is_builder() {
            unsafe {
                std::ptr::copy_nonoverlapping(bb.builder_bytes(), buf.add(pos), bb.builder_len());
            }
            pos += bb.builder_len();
        } else {
            unsafe {
                std::ptr::copy_nonoverlapping(bb.bytes, buf.add(pos), bb.len);
            }
            pos += bb.len;
        }
    }
    pos
}

/// Flatten a rope (cons) box in place: concatenate the leaves into a fresh
/// buffer **in the box's own generation** (old for promoted boxes, so the
/// invariant that a box and its bytes share a generation holds) and rewrite
/// the box to flat. Called on first read; later reads are O(1). Iterative,
/// so arbitrarily deep left-leaning ropes can't overflow the stack.
fn flatten_rope_in_place(box_ptr: *mut AString) {
    let b = unsafe { &*box_ptr };
    if !b.is_cons() {
        return;
    }
    let heap = heap::current_heap();
    let box_addr = box_ptr as usize;
    // The total is cached in the root box — O(1), no tree walk.
    let total = b.len();
    // Allocate the flat buffer in the box's own generation so a promoted
    // rope's bytes survive the next young sweep.
    let buf = unsafe {
        if (*heap).addr_in_old(box_addr) {
            (*heap).alloc_old_bytes_uninit(total)
        } else {
            (*heap).alloc_bytes_uninit(total)
        }
    };
    let pos = fill_string_into(buf, 0, box_addr);
    debug_assert_eq!(pos, total);
    // Rewrite the root box to flat.
    unsafe {
        let root = &mut *box_ptr;
        root.bytes = buf;
        root.len = total;
    }
}

/// Minimum capacity for a fresh string builder's buffer.
const BUILDER_MIN_CAP: usize = 32;

/// A fresh builder box pointing at a growable buffer holding the
/// concatenation of `a` then `b` (both contiguous: flat or builder).
fn builder_start(a: Value, b: Value) -> Value {
    let heap = heap::current_heap();
    let a_box = unsafe { &*heap_ptr::<AString>(a.0) };
    let b_box = unsafe { &*heap_ptr::<AString>(b.0) };
    let a_len = a_box.len();
    let b_len = b_box.len();
    let total = a_len + b_len;
    let cap = (total + 16).max(BUILDER_MIN_CAP);
    let buf = unsafe { (*heap).alloc_raw_region(cap, KIND_STRING as u64) };
    unsafe {
        std::ptr::copy_nonoverlapping(a_box.contiguous_bytes(), buf, a_len);
        std::ptr::copy_nonoverlapping(b_box.contiguous_bytes(), buf.add(a_len), b_len);
    }
    builder_box(buf, total, cap)
}

/// Append contiguous string `b` into builder `a`'s buffer: write `b`'s bytes
/// at the end (never overwriting what's already there, so aliased readers of
/// `a`'s older boxes stay valid), growing the buffer by doubling when full,
/// and hand back a FRESH box — strings are immutable, so the append result
/// is a new box and `a`'s box keeps its prefix. Amortized O(1) per append.
fn builder_append(a: Value, b: Value) -> Value {
    let heap = heap::current_heap();
    let a_box = unsafe { &*heap_ptr::<AString>(a.0) };
    let b_box = unsafe { &*heap_ptr::<AString>(b.0) };
    let b_len = b_box.len();
    let b_bytes = b_box.contiguous_bytes();
    let len = a_box.builder_len();
    let cap = a_box.builder_cap();
    let new_len = len + b_len;
    let (buf, new_cap) = if new_len <= cap {
        (a_box.builder_bytes(), cap)
    } else {
        let new_cap = cap.max(new_len).max(BUILDER_MIN_CAP) * 2;
        let nb = unsafe { (*heap).alloc_raw_region(new_cap, KIND_STRING as u64) };
        unsafe {
            std::ptr::copy_nonoverlapping(a_box.builder_bytes(), nb, len);
        }
        (nb, new_cap)
    };
    unsafe {
        // `b` may alias this very buffer (`s + s`): memmove, not memcpy.
        std::ptr::copy(b_bytes, buf.add(len), b_len);
    }
    builder_box(buf, new_len, new_cap)
}

/// Convert a cons rope `a` into a growable builder and append contiguous
/// `b`: flatten `a`'s content into a fresh buffer with headroom (the cons
/// tree is left untouched, so aliases keep working), then append. One O(n)
/// copy for the conversion, then O(1) appends — the `s = s + leaf` loop
/// shape.
fn cons_to_builder(a: Value, b: Value) -> Value {
    let heap = heap::current_heap();
    let a_box = unsafe { &*heap_ptr::<AString>(a.0) };
    let b_box = unsafe { &*heap_ptr::<AString>(b.0) };
    let a_len = a_box.len();
    let b_len = b_box.len();
    let total = a_len + b_len;
    let cap = (total + 16).max(BUILDER_MIN_CAP);
    let buf = unsafe { (*heap).alloc_raw_region(cap, KIND_STRING as u64) };
    let end = fill_string_into(buf, 0, payload_to_usize(a.0));
    debug_assert_eq!(end, a_len);
    unsafe {
        std::ptr::copy_nonoverlapping(b_box.contiguous_bytes(), buf.add(end), b_len);
    }
    builder_box(buf, total, cap)
}

/// A new builder box over buffer `buf` with `used` bytes and `cap` capacity.
#[inline]
fn builder_box(buf: *mut u8, used: usize, cap: usize) -> Value {
    let heap = heap::current_heap();
    let box_ptr = unsafe { (*heap).alloc_raw_region(32, KIND_STRING as u64) };
    unsafe {
        (box_ptr as *mut u64).write(TAG_OBJ);
        (box_ptr.add(8) as *mut usize).write(used);
        (box_ptr.add(16) as *mut usize).write(buf as usize);
        (box_ptr.add(24) as *mut usize).write(cap);
    }
    Value(TAG_STR | usize_to_payload(box_ptr as usize))
}

/// String + string concatenation: use the growable builder when it helps
/// (already-building, or a rope followed by a fresh single-part leaf), and
/// fall back to a cons node otherwise. Never mutates either operand's box.
fn concat_strings(a: Value, b: Value) -> Value {
    let b_box = unsafe { &*heap_ptr::<AString>(b.0) };
    if !b_box.is_cons() {
        let a_box = unsafe { &*heap_ptr::<AString>(a.0) };
        if a_box.is_builder() {
            return builder_append(a, b);
        }
        if a_box.is_cons() {
            return cons_to_builder(a, b);
        }
        if a_box.len() + b_box.len() <= BUILDER_MIN_CAP {
            // Small flat+flat: start a builder (already-contiguous result).
            return builder_start(a, b);
        }
    }
    Value::rope(a, b)
}

#[inline]
fn heap_ptr<T>(w: u64) -> *const T {
    payload_to_usize(w) as *const T
}

/// Everything reachable through the `TAG_MISC` prefix. Rare or fat-pointer
/// payloads live here behind a single boxed enum.
enum MiscBox {
    Promise(Arc<Mutex<PromiseState>>),
    /// A native function. `proto` is `undefined` for ordinary natives; for
    /// native constructors (Map/Set, Error classes) it is the constructor's
    /// `prototype` object — `new C` builds instances from it and
    /// `o instanceof C` walks the chain against it. `props` holds static
    /// properties (e.g. `String.fromCharCode`) with the same lazy-alloc
    /// shape as closures; `None` until first written.
    Native {
        f: NativeFn,
        proto: Value,
        props: RefCell<Option<Rc<RefCell<hashbrown::HashMap<String, Value>>>>>,
    },
    Pointer(*mut u8),
    Buffer { ptr: *mut u8, len: usize },
    Channel(Arc<Mutex<ChannelState>>),
    /// A regular expression. The compiled program is shared through
    /// `RegexState.compiled` (the VM caches it per pattern+flags); each
    /// literal evaluation gets a FRESH state so `lastIndex` is per-object
    /// like JS (`/a/g` twice are distinct objects with independent
    /// `lastIndex`). The `Mutex` matches the Promise/Channel cross-thread
    /// convention.
    Regex(Arc<Mutex<RegexState>>),
}

/// Per-object regex state: the shared compiled program plus the mutable
/// `lastIndex` cursor used by `/g` and `/y` matching.
pub struct RegexState {
    pub compiled: Arc<regex::RegexCompiled>,
    /// Current `lastIndex` in UTF-16 code units (V8's unit).
    pub last_index: usize,
}

/// The shared state of a message-passing `Channel`: a FIFO of sent messages
/// plus the promises of pending `recv()` callers. `send` resolves the oldest
/// waiter (or buffers); `recv` takes the oldest message (or waits). The VM
/// drives the promise side via its microtask queue, so channels are the
/// "message-passing primitives built into the event loop" from the PRD.
///
/// A message in a channel's queue. Anonymous channels are per-VM (their
/// values never leave the allocating heap) and hold raw `Value`s; **named**
/// channels are shared across VMs through `channel.get`, so their messages
/// travel as serialized bytes — a `Value` is an arena pointer valid only on
/// its allocating thread and would corrupt the receiver's heap if handed
/// over raw. The VM decodes `Bytes` into the receiver's heap on `recv`.
pub enum ChannelItem {
    Raw(Value),
    Bytes(Vec<u8>),
}

pub struct ChannelState {
    pub queue: VecDeque<ChannelItem>,
    pub waiters: VecDeque<Value>,
    /// True when created via `channel.create(name)` — the channel is shared
    /// across VMs, so messages cross thread-local heaps and must be
    /// serialized on send (see `ChannelItem`).
    pub named: bool,
}

impl ChannelState {
    /// Hand `item` to the oldest waiter (returning `(waiter_promise, item)`
    /// so the caller resolves the waiter with it), or buffer it when no one
    /// is waiting yet (returns `None`).
    pub fn send_item(&mut self, item: ChannelItem) -> Option<(Value, ChannelItem)> {
        if let Some(w) = self.waiters.pop_front() {
            Some((w, item))
        } else {
            self.queue.push_back(item);
            None
        }
    }

    /// Dequeue the next item, or `None` when empty.
    pub fn recv_item(&mut self) -> Option<ChannelItem> {
        self.queue.pop_front()
    }

    pub fn push_waiter(&mut self, p: Value) {
        self.waiters.push_back(p);
    }

    pub fn len(&self) -> usize {
        self.queue.len() + self.waiters.len()
    }
}

/// NaN-boxed JavaScript value (see module comment for the layout).
pub struct Value(u64);

/// Backing store for a JS array value. `Ints` is the V8 `PACKED_SMI_ELEMENTS`
/// fast path: dense `i64` elements with no per-element tag decode on read or
/// write — the GC never walks them (plain integers hold no heap references),
/// and reads are a raw load plus one tag OR. Any non-int element written
/// (including a hole, which is `undefined`) lazily converts the whole array
/// to the general `Values` form, so semantic correctness is untouched.
#[derive(Debug)]
pub enum ArrayData {
    Ints(Vec<i64>),
    Values(Vec<Value>),
}

impl ArrayData {
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            ArrayData::Ints(v) => v.len(),
            ArrayData::Values(v) => v.len(),
        }
    }

    /// Element `i` as a `Value` (undefined out of bounds) — the packed path
    /// is a bounds check, an `i64` load, and a tag OR; no clone, no borrow of
    /// element storage beyond the `RefCell` guard the caller holds.
    #[inline]
    pub fn get(&self, i: usize) -> Value {
        match self {
            ArrayData::Ints(v) => match v.get(i) {
                Some(x) => Value::int(*x),
                None => Value::undefined(),
            },
            ArrayData::Values(v) => match v.get(i) {
                Some(x) => x.clone(),
                None => Value::undefined(),
            },
        }
    }

    /// Write `v` at `i` (the caller guarantees `i < len`). A non-int value
    /// escapes the whole array to the general form first.
    #[inline]
    pub fn set(&mut self, i: usize, v: Value) {
        match self {
            ArrayData::Ints(vs) => match v.as_int() {
                Some(x) => vs[i] = x,
                None => {
                    let mut out: Vec<Value> = vs.iter().map(|x| Value::int(*x)).collect();
                    out[i] = v;
                    *self = ArrayData::Values(out);
                }
            },
            ArrayData::Values(vs) => vs[i] = v,
        }
    }

    /// Write `v` at `i`, extending the array with `undefined` holes first if
    /// needed (JS `arr[i] = v` semantics). Extending always escapes — holes
    /// are not representable in the packed form.
    #[inline]
    pub fn set_extend(&mut self, i: usize, v: Value) {
        if i >= self.len() {
            match self {
                ArrayData::Ints(vs) => {
                    let mut out: Vec<Value> = vs.iter().map(|x| Value::int(*x)).collect();
                    out.resize(i + 1, Value::undefined());
                    out[i] = v;
                    *self = ArrayData::Values(out);
                }
                ArrayData::Values(vs) => {
                    vs.resize(i + 1, Value::undefined());
                    vs[i] = v;
                }
            }
            return;
        }
        self.set(i, v);
    }

    /// Append `v`. Ints stay packed when `v` is an int; otherwise the whole
    /// array escapes.
    #[inline]
    pub fn push(&mut self, v: Value) {
        match self {
            ArrayData::Ints(vs) => match v.as_int() {
                Some(x) => vs.push(x),
                None => {
                    let mut out: Vec<Value> = vs.iter().map(|x| Value::int(*x)).collect();
                    out.push(v);
                    *self = ArrayData::Values(out);
                }
            },
            ArrayData::Values(vs) => vs.push(v),
        }
    }

    /// Pop the last element (undefined when empty).
    #[inline]
    pub fn pop(&mut self) -> Value {
        match self {
            ArrayData::Ints(vs) => vs.pop().map(Value::int).unwrap_or(Value::undefined()),
            ArrayData::Values(vs) => vs.pop().unwrap_or(Value::undefined()),
        }
    }

    /// Remove and return the first element (undefined when empty). Ints stay
    /// packed. Uses `rotate_left` + truncate for large arrays to avoid
    /// per-element `remove(0)` overhead when the Vec is heavily reused.
    #[inline]
    pub fn shift(&mut self) -> Value {
        match self {
            ArrayData::Ints(vs) => {
                if vs.is_empty() {
                    Value::undefined()
                } else if vs.len() > 64 {
                    // For large packed arrays, memmove via Vec::drain is faster than remove(0) loop
                    let v = vs[0];
                    vs.rotate_left(1);
                    vs.truncate(vs.len()-1);
                    Value::int(v)
                } else {
                    Value::int(vs.remove(0))
                }
            }
            ArrayData::Values(vs) => {
                if vs.is_empty() {
                    Value::undefined()
                } else if vs.len() > 64 {
                    let v = vs[0].clone();
                    vs.rotate_left(1);
                    vs.truncate(vs.len()-1);
                    v
                } else {
                    vs.remove(0)
                }
            }
        }
    }

    /// Prepend `items` at the front, preserving their order (JS
    /// `unshift(a, b, c)` → `[a, b, c, ...old]`). Returns the new length.
    /// Ints stay packed when every item is an int; otherwise the whole array
    /// escapes.
    pub fn unshift_front(&mut self, items: &[Value]) -> usize {
        let old = self.len();
        if items.iter().all(|v| v.as_int().is_some()) {
            match self {
                ArrayData::Ints(vs) => {
                    for (j, v) in items.iter().enumerate() {
                        vs.insert(j, v.as_int().unwrap());
                    }
                }
                ArrayData::Values(vs) => {
                    for (j, v) in items.iter().enumerate() {
                        vs.insert(j, v.clone());
                    }
                }
            }
        } else {
            match self {
                ArrayData::Ints(vs) => {
                    let mut out: Vec<Value> = vs.iter().map(|x| Value::int(*x)).collect();
                    for (j, v) in items.iter().enumerate() {
                        out.insert(j, v.clone());
                    }
                    *self = ArrayData::Values(out);
                }
                ArrayData::Values(vs) => {
                    for (j, v) in items.iter().enumerate() {
                        vs.insert(j, v.clone());
                    }
                }
            }
        }
        old + items.len()
    }

    /// Snapshot the elements as `Value`s. The packed path materializes each
    /// element (used by spreads, slices, serialization — cold paths where a
    /// per-element tag OR is noise).
    pub fn to_values(&self) -> Vec<Value> {
        match self {
            ArrayData::Ints(vs) => vs.iter().map(|x| Value::int(*x)).collect(),
            ArrayData::Values(vs) => vs.clone(),
        }
    }

    /// Borrow the elements as `Value`s when the array is in the general form
    /// (iteration-heavy callers avoid a copy; the packed path falls back to
    /// `to_values`).
    #[inline]
    pub fn as_values(&self) -> Option<&[Value]> {
        match self {
            ArrayData::Values(vs) => Some(vs),
            ArrayData::Ints(_) => None,
        }
    }

    /// True when every element is an int (the packed form is active).
    #[inline]
    pub fn is_packed(&self) -> bool {
        matches!(self, ArrayData::Ints(_))
    }
}

impl Clone for Value {
    /// String/array/object payloads live in the value heap: cloning is a plain
    /// bit copy (the heap owns them). Cells/functions/misc bump their `Rc`.
    #[inline(always)]
    fn clone(&self) -> Self {
        match self.0 & TAG_MASK {
            TAG_CELL => unsafe {
                Rc::increment_strong_count(heap_ptr::<RefCell<Value>>(self.0))
            },
            TAG_FN => unsafe { Rc::increment_strong_count(heap_ptr::<FunctionData>(self.0)) },
            TAG_MISC => unsafe { Rc::increment_strong_count(heap_ptr::<MiscBox>(self.0)) },
            _ => {}
        }
        Value(self.0)
    }
}

impl Drop for Value {
    /// The value heap bulk-frees its boxes at teardown; only Rc-backed
    /// payloads (cells/functions/misc) release here.
    fn drop(&mut self) {
        match self.0 & TAG_MASK {
            TAG_CELL => unsafe { drop(Rc::from_raw(heap_ptr::<RefCell<Value>>(self.0))) },
            TAG_FN => unsafe { drop(Rc::from_raw(heap_ptr::<FunctionData>(self.0))) },
            TAG_MISC => unsafe { drop(Rc::from_raw(heap_ptr::<MiscBox>(self.0))) },
            _ => {}
        }
    }
}

/// Payload of a closure value. Boxed (well, `Rc`-shared) so `Value` stays 8
/// bytes and closure clones are cheap.
pub struct FunctionData {
    pub program: u32,
    pub ptr: usize,
    /// Number of leading frame slots that are parameter slots (fixed params
    /// plus one for a rest param). `dispatch_call` uses it to fill missing
    /// arguments with `undefined` — without it, a function called with fewer
    /// args than params reads stale stack garbage for the missing params.
    pub params: u8,
    /// True when the function's body references `arguments`: the VM then
    /// snapshots the passed args into the call frame at entry, because the
    /// frame's local slots overwrite the arg region as the body runs.
    pub uses_args: u8,
    pub cells: Vec<Rc<RefCell<Value>>>,
    /// Class/static property store: `prototype` (the class's prototype
    /// object, read by `new` and `instanceof`), static methods, etc. `None`
    /// for ordinary closures — no allocation. Shared across clones, so
    /// `C.prototype = X` is visible everywhere. The outer `RefCell` gives
    /// interior mutability for lazy first-write allocation (the function
    /// lives behind a shared `Rc`, so the `Option` cannot be mutated
    /// through `&FunctionData` otherwise).
    pub props: RefCell<Option<Rc<RefCell<hashbrown::HashMap<String, Value>>>>>,
}

impl Clone for FunctionData {
    fn clone(&self) -> Self {
        Self {
            program: self.program,
            ptr: self.ptr,
            params: self.params,
            uses_args: self.uses_args,
            cells: self.cells.clone(),
            props: self.props.clone(),
        }
    }
}

/// Hidden-class-style shape: maps property names to storage offsets. Shapes are
/// immutable; adding a property transitions the object to a fresh `Rc<Shape>`
/// (name -> len, value appended). Shape `Rc` identity is the monomorphic
/// inline-cache key: equal shape pointer means the same layout, so a cached
/// offset is a valid direct index into the object's `values`.
#[derive(Debug)]
pub struct Shape {
    map: hashbrown::HashMap<String, u32>,
}

impl Shape {
    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn get(&self, name: &str) -> Option<u32> {
        self.map.get(name).copied()
    }

    /// Property names in deterministic (sorted) order, for GetKeys and
    /// serialization. (Hash maps are unordered; the old object storage sorted
    /// keys for GetKeys and serialization already depended on ordering.)
    pub fn keys_sorted(&self) -> Vec<&String> {
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort();
        keys
    }

    /// Property names in JS *insertion* order (offsets are assigned in
    /// insertion order), for JSON.stringify — JSON emits keys in the order
    /// they were added, unlike GetKeys.
    pub fn keys_by_offset(&self) -> Vec<(&String, u32)> {
        let mut v: Vec<(&String, u32)> = self.map.iter().map(|(k, o)| (k, *o)).collect();
        v.sort_by_key(|(_, o)| *o);
        v
    }
}

/// Storage of a plain object: a shared shape plus the values at each offset.
/// Property get/set is `shape.map[name] -> offset` then `values[offset]`; the
/// offset lookup is what the inline cache in the VM bypasses entirely.
///
/// `deleted` is the tombstone set for `delete o.p`: the property's shape entry
/// is kept (so offsets — and the inline caches built on them — stay valid),
/// its value slot becomes undefined, and this flag makes the iteration paths
/// (for-in, serialization) skip it.
#[derive(Debug)]
pub struct ObjectData {
    pub shape: Rc<Shape>,
    pub values: Vec<Value>,
    pub deleted: Vec<bool>,
    /// Prototype-chain head (`undefined` for a plain object). A property
    /// read that misses the own shape falls through to `proto`, then
    /// `proto.proto`, … — this is what makes `o.m()` dispatch to the method
    /// on the class's `prototype` object. Not a shape entry, so iteration and
    /// JSON.stringify never see it.
    pub proto: Value,
    /// Container/error marker: 0 = plain object, 1 = Map, 2 = Set,
    /// 3 = Error instance (display and ToString use `name: message`). When
    /// nonzero, `entries` holds the key/value table and the VM synthesizes
    /// the container methods (`m.get`, `s.add`, …) from it. Not a shape
    /// entry, so for-in and JSON.stringify (which walk the shape) see `{}` —
    /// just like Node.
    pub container: u8,
    /// Map/Set entries, allocated lazily on first insert. Keys use
    /// SameValueZero semantics via [`MapKey`]: `NaN` finds `NaN`, `-0` and
    /// `+0` share a slot, `1` and `1.0` are the same key. The GC walks keys
    /// and values with the rest of the box (promote, dirty scan, mark).
    pub entries: Option<ContainerData>,
    /// Accessor properties (class getters/setters): name → (getter, setter),
    /// `undefined` for the absent side. Consulted by the VM's get/set paths
    /// after the own-shape miss and before the prototype walk; getters/setters
    /// are invoked with the receiver as `this`. Never in the shape, so the
    /// inline caches (which only cache own shape hits) never bypass them.
    pub accessors: Option<hashbrown::HashMap<String, (Value, Value)>>,
}

impl ObjectData {
    #[inline]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.shape.get(name).and_then(|o| {
            if self.deleted[o as usize] {
                None
            } else {
                Some(&self.values[o as usize])
            }
        })
    }

    /// Delete `name` (JS `delete o.name`): tombstone the slot to undefined and
    /// flag it so iteration skips it. Returns whether the property existed.
    #[inline]
    pub fn delete(&mut self, name: &str) -> bool {
        match self.shape.get(name) {
            Some(o) => {
                let o = o as usize;
                self.values[o] = Value::undefined();
                self.deleted[o] = true;
                true
            }
            None => false,
        }
    }

    /// Set `name` to `v`, transitioning to a new shape if the property is new.
    /// Returns the offset written (used to populate the inline cache).
    /// Re-setting a deleted property clears its tombstone flag.
    #[inline]
    pub fn set(&mut self, name: &str, v: Value) -> u32 {
        match self.shape.get(name) {
            Some(o) => {
                let o = o as usize;
                self.values[o] = v;
                self.deleted[o] = false;
                o as u32
            }
            None => {
                let o = self.values.len() as u32;
                let mut map = self.shape.map.clone();
                map.insert(name.to_string(), o);
                self.shape = Rc::new(Shape { map });
                self.values.push(v);
                self.deleted.push(false);
                o
            }
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Identity of this object's shape `Rc` — the inline-cache key.
    #[inline]
    pub fn shape_ptr(&self) -> u64 {
        Rc::as_ptr(&self.shape) as u64
    }

    /// (name, value) pairs in deterministic sorted order (serialization,
    /// printing), skipping deleted properties.
    pub fn iter_sorted(&self) -> Vec<(&str, &Value)> {
        self.shape
            .keys_sorted()
            .into_iter()
            .filter(|k| !self.deleted[self.shape.map[k.as_str()] as usize])
            .map(|k| (k.as_str(), &self.values[self.shape.map[k.as_str()] as usize]))
            .collect()
    }

    /// Shape keys in deterministic sorted order, skipping deleted properties
    /// (for-in / GetKeys over objects).
    pub fn keys_live(&self) -> Vec<&String> {
        self.shape
            .keys_sorted()
            .into_iter()
            .filter(|k| !self.deleted[self.shape.map[k.as_str()] as usize])
            .collect()
    }
}

/// Settlement of a promise. Rejections carry a value but, since the VM has no
/// exceptions yet, awaiting a rejected promise yields `undefined`.
#[derive(Debug, Clone)]
pub enum PromiseStatus {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// Cross-thread wake handle for a VM's event loop, stamped onto every
/// promise the VM creates. A settlement from a *different* VM (a channel
/// send landing on a waiter parked by a worker, or the reverse) must not be
/// enqueued on the resolver's own microtask queue — the waiter's continuation
/// ids index the owner's registry, and the value is an arena pointer valid
/// only on its allocating thread. Instead the resolver serializes the value
/// and hands `(promise, bytes)` to the owner's inbox, then wakes its event
/// loop; the owner decodes into its own heap and resolves locally.
pub struct WakeHandle {
    /// Wake pipe: `deliver` pushes a token so a parked event loop waiting on
    /// `recv_timeout` returns immediately instead of on its poll cadence.
    wake: std::sync::mpsc::Sender<()>,
    /// Incoming routed settlements, drained by the owner VM's event loop.
    inbox: Arc<Mutex<VecDeque<(Arc<Mutex<PromiseState>>, Vec<u8>)>>>,
}

impl WakeHandle {
    /// Create a handle plus the receiver the owning event loop waits on.
    pub fn new() -> (Self, std::sync::mpsc::Receiver<()>) {
        let (wake, rx) = std::sync::mpsc::channel();
        (
            Self {
                wake,
                inbox: Arc::new(Mutex::new(VecDeque::new())),
            },
            rx,
        )
    }

    /// Route a settlement to the owner: buffer `(promise, serialized value)`
    /// and wake its event loop. The value crossed as bytes because `Value`s
    /// are arena pointers valid only on the allocating thread.
    pub fn deliver(&self, promise: Arc<Mutex<PromiseState>>, bytes: Vec<u8>) {
        self.inbox
            .lock()
            .unwrap_or_else(|g| g.into_inner())
            .push_back((promise, bytes));
        // Unbounded pipe: never blocks on capacity; Err only when the
        // owner's receiver is gone (its VM tore down), which is fine.
        let _ = self.wake.send(());
    }

    /// Drain every buffered settlement. Runs on the owner VM thread; the
    /// caller decodes each byte payload into its own heap.
    pub fn take_deliveries(&self) -> Vec<(Arc<Mutex<PromiseState>>, Vec<u8>)> {
        let mut d = self.inbox.lock().unwrap_or_else(|g| g.into_inner());
        std::mem::take(&mut *d).into_iter().collect()
    }
}

impl Clone for WakeHandle {
    fn clone(&self) -> Self {
        Self { wake: self.wake.clone(), inbox: self.inbox.clone() }
    }
}

// Manual Debug (deriving would require PromiseState: Debug, which itself
// requires WakeHandle: Debug — a cycle). The wake pipe is enough to identify
// the handle in traces.
impl fmt::Debug for WakeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WakeHandle").field("wake", &self.wake).finish()
    }
}

/// Shared state of a `Value::Promise`. Continuation ids index into the VM's
/// continuation registry (defined in the VM crate, so kept opaque here).
#[derive(Debug)]
pub struct PromiseState {
    pub status: PromiseStatus,
    pub continuations: Vec<u64>,
    /// Event loop that created this promise (see `WakeHandle`). Settlements
    /// from another VM route back to it instead of being enqueued locally.
    pub owner: Option<Arc<WakeHandle>>,
}

impl PromiseState {
    pub fn pending() -> Self {
        Self { status: PromiseStatus::Pending, continuations: Vec::new(), owner: None }
    }
}

/// Host runtime handle handed to native functions so they can re-enter the VM
/// (e.g. invoke a JavaScript callback captured from the script).
pub trait VmHost {
    fn call_value(&mut self, callee: &Value, args: &[Value]) -> Value;

    /// Call `callee` with an explicit `this` receiver (`Function.prototype
    /// .call`/`.apply`). `None` means `this = undefined`, like `call_value`.
    /// Default: fall back to the no-`this` call.
    fn call_value_with_this(
        &mut self,
        callee: &Value,
        this_arg: Option<Value>,
        args: &[Value],
    ) -> Value {
        let _ = this_arg;
        self.call_value(callee, args)
    }

    /// Unit boundary (an HTTP request finished, a script run ended): the host
    /// may run its escape-analysis pass, promoting live values out of the
    /// young generation and reclaiming the rest. Default: no-op.
    fn promote_generation(&mut self) {}

    /// Incremental-GC write barrier for Rc-backed structures (closure cells,
    /// promises, channels): the VM records the mutation so the next mark
    /// slice re-traces the structure. Default: no-op.
    fn note_gc_dirty(&mut self, _d: RcDirtyRef) {}

    /// Incremental-GC write barrier for arena boxes: set the box's dirty bit
    /// so the next collection's dirty scan re-walks its interior (a native
    /// mutating an old Map/Set's entries must call this — a young value
    /// stored into an old box would otherwise be missed). Default: no-op.
    fn note_box_dirty(&mut self, _addr: usize) {}

    /// Take the VM's pending uncaught exception, if any. The HTTP server uses
    /// this to convert a synchronous throw inside a request handler into a
    /// rejected handler promise (a 500 response) instead of letting it abort
    /// the whole program. Default: nothing pending.
    fn take_uncaught_exception(&mut self) -> Option<Value> {
        None
    }

    /// The `this` receiver of the current native call. Method-call natives
    /// (Map/Set methods installed on the prototype) read their instance from
    /// here — the same shared native serves every instance. A plain call or
    /// no call in progress yields `undefined`. The VM stashes the receiver
    /// for the duration of the native invocation, so re-entrant calls (a
    /// native invoking a JS callback) restore the outer value. Default:
    /// undefined.
    fn this_value(&self) -> Value {
        Value::undefined()
    }

    /// Settle a promise, queuing its continuations as microtasks.
    fn resolve_promise(&mut self, promise: &Value, value: Value) {
        let _ = (promise, value);
    }

    /// Settle a promise as rejected (continuations run with `undefined`).
    fn reject_promise(&mut self, promise: &Value, value: Value) {
        let _ = (promise, value);
    }

    /// Register `.then` callbacks on `promise`; returns the chained promise.
    fn then(&mut self, promise: &Value, callback: Value, on_rejected: Option<Value>) -> Value {
        let _ = (promise, callback, on_rejected);
        Value::undefined()
    }

    /// Schedule `callback` to run after `ms` milliseconds; `period: Some(p)`
    /// makes it a repeating timer (rescheduled every `p` ms). Returns a
    /// numeric handle `clear_timer` accepts.
    fn schedule_timer(&mut self, callback: Value, ms: f64, period: Option<f64>) -> u64 {
        let _ = (callback, ms, period);
        0
    }

    /// Cancel a pending timer by its handle id (returned by `setTimeout` /
    /// `setInterval`). Default: no-op.
    fn clear_timer(&mut self, _id: u64) {}

    /// `queueMicrotask(cb)`: run `cb` after the current synchronous
    /// execution, before any timers. Default: no-op.
    fn queue_microtask(&mut self, callback: Value) {
        let _ = callback;
    }

    /// Throw `exc` from a native: a surrounding try/catch handler (if any)
    /// catches it exactly like `throw exc`; otherwise it becomes the VM's
    /// uncaught exception. Default: no-op.
    fn throw_exception(&mut self, _exc: Value) {}

    /// Invoke `func` in the Python sidecar serving `src`, with `args`
    /// translated onto the wire (shared-segment pointers become offsets).
    /// Returns a promise of the result (rejected on sidecar error). Default:
    /// undefined — a host without a sidecar.
    fn python_call(
        &mut self,
        _src: &str,
        _func: &str,
        _args: &[Value],
        _base: usize,
        _cap: usize,
    ) -> Value {
        Value::undefined()
    }

    /// Run `f` (with serialized `args`) as an isolated task on the event
    /// loop / worker thread; returns a promise of its result. Default:
    /// undefined.
    fn spawn_fn(&mut self, _f: &Value, _args: &[Value]) -> Value {
        Value::undefined()
    }

    /// `finalizePythonEmbed()`: optional clean teardown of the in-process
    /// CPython interpreter (`ALLOY_PYTHON_EMBED=1` only) — release this
    /// VM's python backends and call `Py_FinalizeEx`. Terminal: after it
    /// succeeds, embed mode is off for the process and later `.py` imports
    /// fall back to child sidecars. Returns `{ finalized, error,
    /// liveBackends }`. Default: finalized=false (embed inactive).
    fn finalize_python_embed(&mut self) -> Value {
        let mut m = hashbrown::HashMap::new();
        m.insert("finalized".to_string(), Value::bool(false));
        m.insert("error".to_string(), Value::string("embed mode is not active".to_string()));
        m.insert("liveBackends".to_string(), Value::number(0.0));
        Value::object(m)
    }

    /// Pump pending async work (settled python sidecar calls, microtasks)
    /// until quiet. Hosts that invoke a handler and then need its awaited
    /// results (the HTTP server) call this after the handler suspends.
    /// Default: no-op.
    fn drive_pending(&mut self) {}

    /// `require('./mod.ajs')`: load, run (once, cached), and return the
    /// module's exports object. Throws (via `throw_exception`) on missing
    /// files, syntax errors, and circular requires. Default: undefined — a
    /// host without module loading.
    fn require_module(&mut self, _path: &str) -> Value {
        Value::undefined()
    }

    /// `reload('./mod.ajs')`: drop the cached module so the next require
    /// re-runs it (hot reload). True when a cached module was dropped; false
    /// for builtins and unresolvable paths. Default: false.
    fn reload_module(&mut self, _path: &str) -> bool {
        false
    }

    /// One non-blocking pump of pending async work: settle whatever python
    /// sidecar calls have completed and run one batch of microtasks. A
    /// concurrent server interleaves this with accepting new connections so
    /// one slow handler never stalls the next. Default: no-op.
    fn pump_async(&mut self) {}

    /// Create a promise owned by this VM's event loop: a settlement from
    /// another thread (a channel send landing on a waiter this VM parked) is
    /// routed back to it and wakes it. Default: an unstamped promise — hosts
    /// without cross-thread wake resolve everything locally.
    fn new_promise(&mut self) -> Value {
        Value::promise(Arc::new(Mutex::new(PromiseState::pending())))
    }

    /// This VM's cross-thread wake handle, if it has one. The channel send
    /// path uses it to decide whether a waiter belongs to this event loop
    /// (resolve locally) or another (serialize + route + wake).
    fn wake_handle(&self) -> Option<Arc<WakeHandle>> {
        None
    }

    /// Record that this VM is parked on `p` — a promise exposed to other
    /// threads (a channel `recv` waiter) — so its event loop keeps pumping
    /// until the settlement lands and wakes it. Default: no-op.
    fn park_cross_waiter(&mut self, _p: &Value) {}
}

pub type NativeFn = Arc<dyn Fn(&[Value], &mut dyn VmHost) -> Value + Send + Sync>;

impl Value {
    // ---- constructors ------------------------------------------------------

    #[inline(always)]
    pub fn undefined() -> Value {
        Value(TAG_SMALL | S_UNDEF)
    }
    /// Raw NaN-boxed word. Used for identity comparisons (e.g. the inline
    /// cache's property check: clones of the same heap value share the word).
    #[inline(always)]
    pub fn bits(&self) -> u64 {
        self.0
    }

    /// Rebuild a Value from a raw NaN-boxed word — the inverse of `bits()`.
    /// The word must be a non-heap payload (int/number/small): the VM's
    /// slot-feedback fast paths use it to read a known-int/number local
    /// without the Rc bump that `clone()` performs on cell/fn/misc payloads.
    #[inline(always)]
    pub fn from_raw_word(word: u64) -> Value {
        Value(word)
    }

    /// Raw signed payload of an int-tagged word, without the tag probe (the
    /// caller has established `is_int()` from slot feedback). Same extraction
    /// as `as_int`'s fast path: left-shift drops the tag, arithmetic shift
    /// right sign-extends from bit 47.
    #[inline(always)]
    pub fn int_bits_raw(bits: u64) -> i64 {
        ((bits << 16) as i64) >> 16
    }

    #[inline(always)]
    pub fn null() -> Value {
        Value(TAG_SMALL | S_NULL)
    }
    #[inline(always)]
    pub fn bool(b: bool) -> Value {
        Value(TAG_SMALL | if b { S_TRUE } else { S_FALSE })
    }
    #[inline(always)]
    pub fn number(n: f64) -> Value {
        let bits = n.to_bits();
        if bits & TAG_MASK == TAG_INT {
            // A negative NaN would collide with the tag space.
            Value(CANON_NAN)
        } else {
            Value(bits)
        }
    }
    #[inline(always)]
    pub fn int(i: i64) -> Value {
        if (-1i64 << 47) < i && i < (1i64 << 47) {
            Value(TAG_INT | (i as u64 & PAYLOAD_MASK))
        } else {
            // JS numbers are f64: values outside the small-int tag range fall
            // back to a plain double, NOT a separate int box. Everything up
            // to 2^53 is exact in f64, so a double loses nothing in that
            // range, and beyond 2^53 JS has no wider numeric type anyway —
            // the value is inherently rounded, and a double is the correct
            // representation. Storing these as a tagged misc box made them
            // invisible to `is_int()`/`is_number()` (e.g. a literal
            // `9007199254740992.toString` resolved to undefined). (BigInt
            // literals are a separate, future feature and are rejected at
            // compile time today.)
            Value::number(i as f64)
        }
    }
    #[inline]
    pub fn symbol(id: u64) -> Value {
        Value(TAG_SMALL | S_SYMBOL | ((id & PAYLOAD_MASK) << 4))
    }
    /// Allocate a string from the active value heap: bytes and the `AString`
    /// box both live in the arena, so the string has no per-object allocation
    /// and its drop is a no-op.
    #[inline]
    pub fn string(s: String) -> Value {
        let heap = heap::current_heap();
        let bytes = unsafe { (*heap).alloc_bytes(s.as_bytes()) };
        let len = s.len();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_STRING, AString { bytes, len }) };
        Value(TAG_STR | usize_to_payload(box_ptr as usize))
    }

    /// One ASCII byte as a string, written straight into the arena (the hot
    /// `s[i]` index path — no temporary `String`).
    #[inline]
    pub fn char_str(b: u8) -> Value {
        let heap = heap::current_heap();
        let bytes = unsafe { (*heap).alloc_bytes_uninit(1) };
        unsafe {
            *bytes = b;
        }
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_STRING, AString { bytes, len: 1 }) };
        Value(TAG_STR | usize_to_payload(box_ptr as usize))
    }

    /// A UTF-8 character as a string, written straight into the arena.
    #[inline]
    pub fn char_str_utf8(c: char) -> Value {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        let heap = heap::current_heap();
        let bytes = unsafe { (*heap).alloc_bytes(s.as_bytes()) };
        let len = s.len();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_STRING, AString { bytes, len }) };
        Value(TAG_STR | usize_to_payload(box_ptr as usize))
    }

    /// Concatenate two string slices straight into the arena — no temporary
    /// `String`, no copy — for callers that already hold flat slices.
    #[inline]
    pub fn string_concat2(a: &str, b: &str) -> Value {
        let heap = heap::current_heap();
        let total = a.len() + b.len();
        let bytes = unsafe { (*heap).alloc_bytes_uninit(total) };
        unsafe {
            std::ptr::copy_nonoverlapping(a.as_ptr(), bytes, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), bytes.add(a.len()), b.len());
        }
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_STRING, AString { bytes, len: total }) };
        Value(TAG_STR | usize_to_payload(box_ptr as usize))
    }

    /// Build a rope (ConsString): an O(1) concat that references two string
    /// operands without copying any bytes. The concatenated bytes are
    /// materialized lazily (flattened) the first time the string is read.
    /// The cons box is 32 bytes: the two child slots plus the cached total
    /// length, so repeated `.length` on a growing rope is O(1) too.
    #[inline]
    pub fn rope(a: Value, b: Value) -> Value {
        debug_assert!(a.is_string() && b.is_string(), "rope operands must be strings");
        let heap = heap::current_heap();
        let box_ptr = unsafe { (*heap).alloc_raw_region(32, KIND_STRING as u64) as *mut AString };
        unsafe {
            (*box_ptr).bytes = a.0 as *mut u8;
            (*box_ptr).len = b.0 as usize;
            // O(1) total: both children report their length from a field
            // (flat) or the cache (cons) — never a tree walk.
            ((box_ptr as *mut u8).add(16) as *mut usize).write(a.str_len() + b.str_len());
        }
        Value(TAG_STR | usize_to_payload(box_ptr as usize))
    }

    /// Byte length of a string **without** flattening — O(1) in both forms
    /// (flat reads the field, cons reads the cached total). Used by rope
    /// construction; distinct from `as_str().len()` which flattens first.
    #[inline]
    pub fn str_len(&self) -> usize {
        let b = unsafe { &*heap_ptr::<AString>(self.0) };
        b.len()
    }
    /// Allocate an array box from the active value heap. All-int element
    /// lists pack into the `Ints` fast path (V8's `PACKED_SMI_ELEMENTS`);
    /// anything else uses the general `Values` store.
    #[inline]
    pub fn array(v: Vec<Value>) -> Value {
        let data = if v.iter().all(|e| e.as_int().is_some()) {
            ArrayData::Ints(v.iter().map(|e| e.as_int().unwrap()).collect())
        } else {
            ArrayData::Values(v)
        };
        let heap = heap::current_heap();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_ARRAY, RefCell::new(data)) };
        Value(TAG_ARR | usize_to_payload(box_ptr as usize))
    }

    /// Allocate an array box from raw ints, skipping the check — the caller
    /// already knows every element fits the packed form.
    #[inline]
    pub fn array_ints(v: Vec<i64>) -> Value {
        let heap = heap::current_heap();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_ARRAY, RefCell::new(ArrayData::Ints(v))) };
        Value(TAG_ARR | usize_to_payload(box_ptr as usize))
    }

    /// Allocate an empty array box (packed-int by default: `[]` is an empty
    /// int array and escapes to the general form on the first non-int write).
    #[inline]
    pub fn array_empty() -> Value {
        let heap = heap::current_heap();
        let box_ptr =
            unsafe { (*heap).alloc_box_kind(KIND_ARRAY, RefCell::new(ArrayData::Ints(Vec::new()))) };
        Value(TAG_ARR | usize_to_payload(box_ptr as usize))
    }
    #[inline]
    pub fn object(m: hashbrown::HashMap<String, Value>) -> Value {
        Self::object_ordered(m.into_iter().collect())
    }

    /// Object constructor that preserves the caller's key order in the shape
    /// (offsets are assigned in the provided order), so JSON.stringify and
    /// any ordered iteration see JS insertion order. `object` (HashMap-based)
    /// delegates here — a HashMap's iteration order is arbitrary either way.
    pub fn object_ordered(entries: Vec<(String, Value)>) -> Value {
        Self::object_ordered_with_proto(entries, Value::undefined())
    }

    /// Object with a prototype chain head (`proto` is `undefined` for a
    /// plain object). Class instances are created this way: property reads
    /// miss the own shape and fall through to the proto chain, which is what
    /// makes `o.m()` dispatch to the method on `C.prototype`.
    pub fn object_ordered_with_proto(
        entries: Vec<(String, Value)>,
        proto: Value,
    ) -> Value {
        let mut shape_map =
            hashbrown::HashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let mut values = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            shape_map.insert(k, values.len() as u32);
            values.push(v);
        }
        let n = values.len();
        let od = ObjectData {
            shape: Rc::new(Shape { map: shape_map }),
            values,
            deleted: vec![false; n],
            proto,
            container: 0,
            entries: None,
            accessors: None,
        };
        let heap = heap::current_heap();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_OBJECT, RefCell::new(od)) };
        Value(TAG_OBJ | usize_to_payload(box_ptr as usize))
    }

    /// A bare object whose prototype chain head is `proto` (class instances).
    #[inline]
    pub fn object_with_proto(proto: Value) -> Value {
        Self::object_ordered_with_proto(Vec::new(), proto)
    }

    /// A Map (`container` 1) or Set (`container` 2) instance: an arena
    /// `ObjectData` box with a prototype and a lazily-allocated SameValueZero
    /// entry table. Method reads (`m.get`, `s.add`, …) are synthesized from
    /// the table in the VM, exactly like array methods are.
    pub fn map(proto: Value, container: u8) -> Value {
        let od = ObjectData {
            shape: Rc::new(Shape {
                map: hashbrown::HashMap::new(),
            }),
            values: Vec::new(),
            deleted: Vec::new(),
            proto,
            container,
            entries: None,
            accessors: None,
        };
        let heap = heap::current_heap();
        let box_ptr = unsafe { (*heap).alloc_box_kind(KIND_OBJECT, RefCell::new(od)) };
        Value(TAG_OBJ | usize_to_payload(box_ptr as usize))
    }

    /// Is this value object-like — a thing a `new` constructor may return to
    /// override the fresh instance (JS: objects, arrays, functions)?
    #[inline]
    pub fn is_object_like(&self) -> bool {
        matches!(self.0 & TAG_MASK, TAG_OBJ | TAG_ARR | TAG_FN | TAG_MISC)
    }
    #[inline]
    pub fn cell(inner: Value) -> Value {
        let rc = Rc::new(RefCell::new(inner));
        Value(TAG_CELL | own_rc(rc))
    }
    /// Wrap an existing cell `Rc` (sharing it, not copying it), for the
    /// closure machinery that needs the slot and the captured value to observe
    /// the same cell.
    #[inline]
    pub fn cell_rc(rc: Rc<RefCell<Value>>) -> Value {
        Value(TAG_CELL | own_rc(rc))
    }
    /// Extract a strong `Rc` clone of the cell backing this value (the value
    /// keeps its own reference; both are released independently).
    #[inline]
    pub fn as_cell_rc(&self) -> Option<Rc<RefCell<Value>>> {
        if self.0 & TAG_MASK == TAG_CELL {
            let ptr = heap_ptr::<RefCell<Value>>(self.0);
            unsafe {
                Rc::increment_strong_count(ptr);
                Some(Rc::from_raw(ptr))
            }
        } else {
            None
        }
    }
    #[inline]
    pub fn function(fd: FunctionData) -> Value {
        let rc = Rc::new(fd);
        Value(TAG_FN | own_rc(rc))
    }
    #[inline]
    pub fn native(f: NativeFn) -> Value {
        Value::misc(MiscBox::Native {
            f,
            proto: Value::undefined(),
            props: RefCell::new(None),
        })
    }

    /// A native constructor: like [`Value::native`], but carrying the
    /// `prototype` object its instances get. Used by `Map` and `Set` so
    /// `new Map()` and `m instanceof Map` work without a bytecode body.
    #[inline]
    pub fn native_ctor(f: NativeFn, proto: Value) -> Value {
        Value::misc(MiscBox::Native {
            f,
            proto,
            props: RefCell::new(None),
        })
    }

    /// A callable native with a prototype and static properties
    /// (`String(x)` plus `String.fromCharCode`): the statics live in the
    /// same lazy props map closures use, read through the native's property
    /// path.
    #[inline]
    pub fn native_with_props(
        f: NativeFn,
        proto: Value,
        props: Vec<(String, Value)>,
    ) -> Value {
        let map = Rc::new(RefCell::new(props.into_iter().collect::<hashbrown::HashMap<_, _>>()));
        Value::misc(MiscBox::Native {
            f,
            proto,
            props: RefCell::new(Some(map)),
        })
    }
    #[inline]
    pub fn pointer(p: *mut u8) -> Value {
        Value::misc(MiscBox::Pointer(p))
    }
    #[inline]
    pub fn buffer(ptr: *mut u8, len: usize) -> Value {
        Value::misc(MiscBox::Buffer { ptr, len })
    }
    #[inline]
    pub fn channel(state: Arc<Mutex<ChannelState>>) -> Value {
        Value::misc(MiscBox::Channel(state))
    }
    #[inline]
    pub fn promise(p: Arc<Mutex<PromiseState>>) -> Value {
        Value::misc(MiscBox::Promise(p))
    }

    /// A fresh regex value: shared compiled program, per-object `lastIndex`.
    #[inline]
    pub fn regex(compiled: Arc<regex::RegexCompiled>) -> Value {
        Value::misc(MiscBox::Regex(Arc::new(Mutex::new(RegexState {
            compiled,
            last_index: 0,
        }))))
    }

    fn misc(m: MiscBox) -> Value {
        let rc = Rc::new(m);
        Value(TAG_MISC | own_rc(rc))
    }

    // ---- tag helpers -------------------------------------------------------

    #[inline(always)]
    fn is_tagged(&self) -> bool {
        self.0 & TAG_MASK >= TAG_INT
    }

    #[inline(always)]
    fn as_misc(&self) -> Option<&MiscBox> {
        if self.0 & TAG_MASK == TAG_MISC {
            Some(unsafe { &*heap_ptr(self.0) })
        } else {
            None
        }
    }

    /// Coarse "same category" test used by `===` (fine-grained equality then
    /// comes from `equal`).
    pub fn same_type(&self, other: &Value) -> bool {
        let t = self.0 & TAG_MASK;
        if t != other.0 & TAG_MASK {
            return false;
        }
        if t == TAG_MISC {
            match (self.as_misc(), other.as_misc()) {
                (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
                _ => false,
            }
        } else {
            true
        }
    }

    /// SameValueZero — the key-equality of Map/Set (and of
    /// `Array.prototype.includes`): `NaN` equals `NaN`, `-0` equals `+0`,
    /// ints and f64s compare numerically (`1 === 1.0`), strings by content,
    /// everything else by identity. Node-verified: `new Set([NaN]).has(NaN)`
    /// is true, `new Map().set(1, "a").get(1.0)` is `"a"`.
    pub fn same_value_zero(&self, other: &Value) -> bool {
        if self.0 == other.0 {
            // Identical bits: same small, same box address, same f64.
            return true;
        }
        if self.is_int() {
            if other.is_int() {
                return false;
            }
            if other.is_number() {
                return (self.as_int().unwrap() as f64) == other.as_number().unwrap();
            }
            return false;
        }
        if self.is_number() {
            let x = self.as_number().unwrap();
            if other.is_int() {
                return x == (other.as_int().unwrap() as f64);
            }
            if other.is_number() {
                let y = other.as_number().unwrap();
                return x == y || (x.is_nan() && y.is_nan());
            }
            return false;
        }
        if let Some(s) = self.as_str() {
            return match other.as_str() {
                Some(t) => s == t,
                None => false,
            };
        }
        false
    }

    /// Stable 64-bit hash matching [`Value::same_value_zero`]: numbers hash
    /// by their f64 bit pattern (ints widened, `-0` canonicalized to `+0`,
    /// every NaN to one constant), strings by content (FNV-1a), smalls by
    /// their bits, and objects/arrays/functions/cells/misc by identity (tag +
    /// address). The result is run through a murmur-style finalizer: the raw
    /// bit pattern of an integer-valued double has its low 48 bits zero, so
    /// without the mix every int key lands in the same bucket and linear
    /// probing degenerates to O(n) per insert. Hash collisions are harmless —
    /// `MapKey::eq` disambiguates.
    pub fn hash_key(&self) -> u64 {
        let h = self.hash_key_raw();
        mix_hash(h)
    }

    /// Unmixed hash (see [`Value::hash_key`] for the per-type scheme).
    fn hash_key_raw(&self) -> u64 {
        if self.is_int() {
            (self.as_int().unwrap() as f64).to_bits()
        } else if self.is_number() {
            let x = self.as_number().unwrap();
            let bits = x.to_bits();
            if x.is_nan() {
                0x7FF8_0000_0000_0000 | 0x5555
            } else if bits == 0x8000_0000_0000_0000 {
                // -0.0 and +0.0 are the same Map key.
                0
            } else {
                bits
            }
        } else if let Some(s) = self.as_str() {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in s.as_bytes() {
                h ^= *byte as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        } else {
            // Small, object, array, function, cell, misc: identity.
            self.0
        }
    }

    // ---- type predicates ---------------------------------------------------

    #[inline(always)]
    pub fn is_undefined(&self) -> bool {
        self.0 == TAG_SMALL | S_UNDEF
    }
    #[inline(always)]
    pub fn is_null(&self) -> bool {
        self.0 == TAG_SMALL | S_NULL
    }
    #[inline(always)]
    pub fn is_number(&self) -> bool {
        !self.is_tagged()
    }
    #[inline(always)]
    pub fn is_int(&self) -> bool {
        self.0 & TAG_MASK == TAG_INT
    }
    #[inline(always)]
    pub fn is_string(&self) -> bool {
        self.0 & TAG_MASK == TAG_STR
    }
    #[inline(always)]
    pub fn is_array(&self) -> bool {
        self.0 & TAG_MASK == TAG_ARR
    }
    #[inline(always)]
    pub fn is_object(&self) -> bool {
        self.0 & TAG_MASK == TAG_OBJ
    }
    #[inline(always)]
    pub fn is_function(&self) -> bool {
        self.0 & TAG_MASK == TAG_FN
    }
    #[inline(always)]
    pub fn is_native(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Native { .. }))
    }
    #[inline(always)]
    pub fn is_cell(&self) -> bool {
        self.0 & TAG_MASK == TAG_CELL
    }
    #[inline(always)]
    pub fn is_promise(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Promise(_)))
    }
    #[inline(always)]
    pub fn is_pointer(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Pointer(_)))
    }
    #[inline(always)]
    pub fn is_buffer(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Buffer { .. }))
    }
    #[inline(always)]
    pub fn is_channel(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Channel(_)))
    }
    #[inline(always)]
    pub fn is_regex(&self) -> bool {
        matches!(self.as_misc(), Some(MiscBox::Regex(_)))
    }

    // ---- accessors ---------------------------------------------------------

    #[inline(always)]
    pub fn as_bool(&self) -> Option<bool> {
        if self.0 & TAG_MASK == TAG_SMALL {
            match self.0 & 0xF {
                S_FALSE => Some(false),
                S_TRUE => Some(true),
                _ => None,
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_symbol(&self) -> Option<u64> {
        if self.0 & TAG_MASK == TAG_SMALL && self.0 & 0xF == S_SYMBOL {
            Some((self.0 & PAYLOAD_MASK) >> 4)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_number(&self) -> Option<f64> {
        if self.is_tagged() {
            None
        } else {
            Some(f64::from_bits(self.0))
        }
    }

    #[inline(always)]
    pub fn as_int(&self) -> Option<i64> {
        if self.0 & TAG_MASK == TAG_INT {
            Some(((self.0 << 16) as i64) >> 16)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_str(&self) -> Option<&str> {
        if self.0 & TAG_MASK == TAG_STR {
            let ptr = heap_ptr::<AString>(self.0);
            let b = unsafe { &*ptr };
            if b.is_cons() {
                // Lazy flatten: materialize the concatenated bytes and
                // rewrite the box to flat. Only the first read pays O(n);
                // later reads are O(1). Strings are immutable from JS's
                // perspective, so the in-place rewrite is invisible.
                flatten_rope_in_place(ptr as *mut AString);
            }
            let b = unsafe { &*ptr };
            let bytes = unsafe { std::slice::from_raw_parts(b.contiguous_bytes(), b.len()) };
            Some(unsafe { std::str::from_utf8_unchecked(bytes) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_array(&self) -> Option<&RefCell<ArrayData>> {
        if self.0 & TAG_MASK == TAG_ARR {
            Some(unsafe { &*heap_ptr(self.0) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_object(&self) -> Option<&RefCell<ObjectData>> {
        if self.0 & TAG_MASK == TAG_OBJ {
            Some(unsafe { &*heap_ptr(self.0) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_cell(&self) -> Option<&RefCell<Value>> {
        if self.0 & TAG_MASK == TAG_CELL {
            Some(unsafe { &*heap_ptr(self.0) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_function(&self) -> Option<&FunctionData> {
        if self.0 & TAG_MASK == TAG_FN {
            Some(unsafe { &*heap_ptr(self.0) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_native(&self) -> Option<&NativeFn> {
        match self.as_misc() {
            Some(MiscBox::Native { f, .. }) => Some(f),
            _ => None,
        }
    }

    /// The prototype a native constructor carries (`undefined` for ordinary
    /// natives). `new C` and `o instanceof C` consult it.
    #[inline]
    pub fn as_native_proto(&self) -> Option<Value> {
        match self.as_misc() {
            Some(MiscBox::Native { proto, .. }) => Some(proto.clone()),
            _ => None,
        }
    }

    /// The static-property map of a native (None when never written).
    pub fn as_native_props(
        &self,
    ) -> Option<&RefCell<Option<Rc<RefCell<hashbrown::HashMap<String, Value>>>>>> {
        match self.as_misc() {
            Some(MiscBox::Native { props, .. }) => Some(props),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn as_promise(&self) -> Option<&Arc<Mutex<PromiseState>>> {
        match self.as_misc() {
            Some(MiscBox::Promise(p)) => Some(p),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn as_pointer(&self) -> Option<*mut u8> {
        match self.as_misc() {
            Some(MiscBox::Pointer(p)) => Some(*p),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn as_buffer(&self) -> Option<(*mut u8, usize)> {
        match self.as_misc() {
            Some(MiscBox::Buffer { ptr, len }) => Some((*ptr, *len)),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn as_channel(&self) -> Option<&Arc<Mutex<ChannelState>>> {
        match self.as_misc() {
            Some(MiscBox::Channel(c)) => Some(c),
            _ => None,
        }
    }

    #[inline(always)]
    pub fn as_regex(&self) -> Option<&Arc<Mutex<RegexState>>> {
        match self.as_misc() {
            Some(MiscBox::Regex(r)) => Some(r),
            _ => None,
        }
    }

    // ---- semantics ---------------------------------------------------------

    #[inline]
    pub fn is_truthy(&self) -> bool {
        let tag = self.0 & TAG_MASK;
        if tag == TAG_SMALL {
            match self.0 & 0xF {
                S_UNDEF | S_NULL | S_FALSE => false,
                _ => true,
            }
        } else if tag == TAG_INT {
            self.as_int().map(|i| i != 0).unwrap_or(true)
        } else if !self.is_tagged() {
            let n = f64::from_bits(self.0);
            n != 0.0 && !n.is_nan()
        } else if tag == TAG_STR {
            !self.as_str().unwrap_or("").is_empty()
        } else {
            true
        }
    }

    pub fn type_name(&self) -> &'static str {
        if self.is_undefined() {
            "undefined"
        } else if self.is_null() {
            // JS: `typeof null` is "object" (a legacy quirk we match).
            "object"
        } else if self.as_bool().is_some() {
            "boolean"
        } else if self.is_number() || self.as_int().is_some() {
            "number"
        } else if self.is_string() {
            "string"
        } else if self.0 & TAG_MASK == TAG_SMALL && self.0 & 0xF == S_SYMBOL {
            "symbol"
        } else if self.is_array() || self.is_object() || self.is_promise() || self.is_regex() {
            "object"
        } else if self.is_function() || self.is_native() {
            "function"
        } else if self.is_pointer() {
            "pointer"
        } else if self.is_buffer() {
            "shared_buffer"
        } else if self.is_cell() {
            "cell"
        } else {
            "unknown"
        }
    }

    #[inline]
    pub fn to_number(&self) -> f64 {
        if let Some(n) = self.as_number() {
            n
        } else if let Some(b) = self.as_bool() {
            if b { 1.0 } else { 0.0 }
        } else if let Some(i) = self.as_int() {
            i as f64
        } else if self.is_null() {
            0.0
        } else if let Some(s) = self.as_str() {
            js_string_to_number(s)
        } else if self.is_object() {
            // A Date object coerces to its epoch milliseconds (like Node);
            // other objects/arrays go through the default ToPrimitive
            // (`[] - 1` is -1 because "" is 0; `{} - 1` is NaN).
            if let Some(ms) = date_ms(self) {
                ms
            } else {
                to_primitive_default(self).to_number()
            }
        } else if self.is_array() {
            to_primitive_default(self).to_number()
        } else {
            f64::NAN
        }
    }

    /// Bitwise AND (`a & b`): both operands go through ToInt32, the result is
    /// an Int32 (JS semantics — bitwise ops always produce signed 32-bit ints).
    #[inline]
    pub fn bitand(&self, other: &Value) -> Value {
        // SMI fast path: ToInt32 on an exact integer is a low-32-bit
        // truncation, so int & int needs no f64 round-trip.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int((a as i32 & b as i32) as i64);
        }
        Value::int((to_int32_value(self.to_number()) & to_int32_value(other.to_number())) as i64)
    }

    /// Bitwise OR (`a | b`), see `bitand`.
    #[inline]
    pub fn bitor(&self, other: &Value) -> Value {
        // SMI fast path (see `bitand`).
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int((a as i32 | b as i32) as i64);
        }
        Value::int((to_int32_value(self.to_number()) | to_int32_value(other.to_number())) as i64)
    }

    /// Bitwise XOR (`a ^ b`), see `bitand`.
    #[inline]
    pub fn bitxor(&self, other: &Value) -> Value {
        // SMI fast path (see `bitand`).
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int((a as i32 ^ b as i32) as i64);
        }
        Value::int((to_int32_value(self.to_number()) ^ to_int32_value(other.to_number())) as i64)
    }

    /// Bitwise NOT (`~a`): ToInt32 then invert all 32 bits.
    #[inline]
    pub fn bitnot(&self) -> Value {
        Value::int((!to_int32_value(self.to_number())) as i64)
    }

    /// Left shift (`a << b`): ToInt32(a) shifted left by (ToUint32(b) & 31),
    /// result is a signed Int32.
    #[inline]
    pub fn shl(&self, other: &Value) -> Value {
        // SMI fast path: ToInt32(a) is `a as i32` for an exact integer, and
        // the shift count's low 5 bits match the f64 path.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int((a as i32).wrapping_shl(b as u32 & 31) as i64);
        }
        let a = to_int32_value(self.to_number());
        let cnt = to_int32_value(other.to_number()) as u32 & 31;
        Value::int(a.wrapping_shl(cnt) as i64)
    }

    /// Signed right shift (`a >> b`): ToInt32(a) shifted right, sign-extending.
    #[inline]
    pub fn shr(&self, other: &Value) -> Value {
        // SMI fast path (see `shl`).
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int((a as i32).wrapping_shr(b as u32 & 31) as i64);
        }
        let a = to_int32_value(self.to_number());
        let cnt = to_int32_value(other.to_number()) as u32 & 31;
        Value::int(a.wrapping_shr(cnt) as i64)
    }

    /// Unsigned right shift (`a >>> b`): ToUint32(a) shifted right, filling
    /// with zeros — the result is an unsigned 32-bit value (0..4294967295).
    #[inline]
    pub fn ushr(&self, other: &Value) -> Value {
        // SMI fast path (see `shl`); the result is an unsigned 32-bit int.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::int(((a as u32).wrapping_shr(b as u32 & 31)) as i64);
        }
        let a = to_int32_value(self.to_number());
        let cnt = to_int32_value(other.to_number()) as u32 & 31;
        Value::int(((a as u32).wrapping_shr(cnt)) as i64)
    }

    /// Exponentiation (`a ** b`), per ES `Number::exponentiate`. Int/int
    /// stays int when the (JS-representable) result fits; results beyond 2^53
    /// round to f64 like V8 (`9 ** 17` is 16677181699666570, not the exact
    /// integer). The f64 fallback implements the spec's special cases that
    /// IEEE `pow` gets wrong for JS: a NaN exponent is always NaN (IEEE
    /// pow(1, NaN) = 1), and (-1) ** ±Infinity is NaN (IEEE gives 1).
    #[inline]
    pub fn pow(&self, other: &Value) -> Value {
        // int ** int: stay int when the result is exactly representable,
        // otherwise fall through to f64 (negative exponents, `2 ** 63`, huge
        // counts — all resolve here). Note the > 2^53 case goes through the
        // same powf as JS: V8 computes `9 ** 17` via fdlibm as
        // 16677181699666570, one ulp above the correctly-rounded integer, and
        // Rust's libm agrees with V8 on these — rounding the exact integer
        // (`as f64`) would give the *wrong* engine-differential value.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            if b >= 0 && b <= u32::MAX as i64 {
                if let Some(r) = a.checked_pow(b as u32) {
                    if r.unsigned_abs() <= (1u64 << 53) {
                        return Value::int(r);
                    }
                    return Value::number(js_powf(a as f64, b as f64));
                }
            }
            return Value::number(js_powf(a as f64, b as f64));
        }
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(js_powf(a, b))
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(js_powf(a, b as f64))
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(js_powf(a as f64, b))
        } else {
            // JS coercion: `true ** 20` is 1, `"2" ** 3` is 8.
            Value::number(js_powf(self.to_number(), other.to_number()))
        }
    }

    #[inline]
    pub fn add(&self, other: &Value) -> Value {
        // SMI fast path: int + int stays in the i64 lane — one tag check per
        // operand, raw i64 add, js_int overflow/2^53 rounding. Collatz/fib/
        // loop-style integer loops never enter the string/coercion machinery.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return match a.checked_add(b) {
                Some(r) => js_int(r),
                None => Value::number(a as f64 + b as f64),
            };
        }
        // Strings, then objects/arrays, concat via their JS string form;
        // otherwise ToNumber both (so `true + 1` is 2, `null + 5` is 5,
        // `undefined + 1` is NaN).
        if self.is_string() && other.is_string() {
            // Fast path: both are strings — growable builder when it helps,
            // otherwise a rope (O(1), no byte copies). Never mutates either
            // operand's box, so aliases stay valid.
            concat_strings(Value(self.0), Value(other.0))
        } else if self.is_string() {
            // Mixed concat: string + anything — convert the other side to
            // its JS string form and concat (growable builder when it helps,
            // rope otherwise), so a growing accumulator (`s = s + i` in a
            // loop) stays O(1) instead of re-copying the whole prefix.
            concat_strings(Value(self.0), Value::string(js_concat_str(other)))
        } else if other.is_string() {
            concat_strings(Value::string(js_concat_str(self)), Value(other.0))
        } else if self.is_array() || self.is_object() || other.is_array() || other.is_object() {
            Value::string(format!("{}{}", js_concat_str(self), js_concat_str(other)))
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(a + b)
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(a + b as f64)
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(a as f64 + b)
        } else {
            Value::number(self.to_number() + other.to_number())
        }
    }

    #[inline]
    pub fn subtract(&self, other: &Value) -> Value {
        // SMI fast path (see `add`): int - int stays in the i64 lane.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return match a.checked_sub(b) {
                Some(r) => js_int(r),
                None => Value::number(a as f64 - b as f64),
            };
        }
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(a - b)
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(a - b as f64)
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(a as f64 - b)
        } else {
            // JS coercion: `"5" - 2` is 3, `[] - 1` is -1, `true - 1` is 0.
            Value::number(self.to_number() - other.to_number())
        }
    }

    #[inline]
    pub fn multiply(&self, other: &Value) -> Value {
        // SMI fast path (see `add`): int * int stays in the i64 lane.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return match a.checked_mul(b) {
                Some(0) if (a < 0) != (b < 0) => {
                    // JS: the product's sign is the XOR of the operands'
                    // signs, so `0 * -5` is -0 (and `1 / -0` is -Infinity).
                    Value::number(-0.0)
                }
                Some(r) => js_int(r),
                None => Value::number(a as f64 * b as f64),
            };
        }
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(a * b)
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(a * b as f64)
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(a as f64 * b)
        } else {
            // JS coercion: `"2" * 3` is 6, `false * 5` is 0.
            Value::number(self.to_number() * other.to_number())
        }
    }

    #[inline]
    pub fn divide(&self, other: &Value) -> Value {
        // SMI fast path: int / int is always an f64 in JS (2 / 2 is 1.0) —
        // skip the string/coercion probes entirely.
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            return Value::number(a as f64 / b as f64);
        }
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(a / b)
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(a / b as f64)
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(a as f64 / b)
        } else {
            // JS coercion: `3 / false` is Infinity, `"6" / "3"` is 2.
            Value::number(self.to_number() / other.to_number())
        }
    }

    /// Modulo (`a % b`): int/int stays int (with `% 0` → NaN, not a panic),
    /// everything else coerces via ToNumber per JS.
    #[inline]
    pub fn modulo(&self, other: &Value) -> Value {
        if let (Some(a), Some(b)) = (self.as_int(), other.as_int()) {
            if b == 0 {
                Value::number(f64::NAN)
            } else if a % b == 0 && a < 0 {
                // JS: the remainder's sign follows the dividend, so `-9 % 3`
                // is -0 (and `1 / (-9 % 3)` is -Infinity).
                Value::number(-0.0)
            } else {
                Value::int(a % b)
            }
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            Value::number(a % b)
        } else if let (Some(a), Some(b)) = (self.as_number(), other.as_int()) {
            Value::number(a % b as f64)
        } else if let (Some(a), Some(b)) = (self.as_int(), other.as_number()) {
            Value::number(a as f64 % b)
        } else {
            // JS coercion: `5 % "2"` is 1, `"5" % 0` is NaN.
            Value::number(self.to_number() % other.to_number())
        }
    }

    /// Unary minus (`-a`): negates int/number, otherwise coerces via ToNumber
    /// (`-true` is -1, `-"5"` is -5, `-undefined` is NaN). `-(0)` is -0 in JS
    /// — the sign matters downstream (`1 / -(0)` is -Infinity).
    #[inline]
    pub fn negate(&self) -> Value {
        if let Some(n) = self.as_number() {
            Value::number(-n)
        } else if let Some(i) = self.as_int() {
            if i == 0 {
                Value::number(-0.0)
            } else {
                match i.checked_neg() {
                    Some(r) => Value::int(r),
                    None => Value::number(-(i as f64)),
                }
            }
        } else {
            Value::number(-self.to_number())
        }
    }

    /// Loose equality (`==`), the ES Abstract Equality comparison. Cross-type
    /// operands are coerced per the spec (number/string, boolean→number,
    /// object/array→default primitive); same-type compares are strict.
    #[inline]
    pub fn equal(&self, other: &Value) -> Value {
        Value::bool(abstract_eq(self, other))
    }
}

/// JS integer semantics: an integer is exactly representable in f64 only up
/// to 2^53. Beyond that, JS arithmetic rounds to the nearest double (V8
/// prints `9 ** 17` as 16677181699666570, not the exact 16677181699666569),
/// so int results beyond ±2^53 become f64 values.
#[inline]
fn js_int(v: i64) -> Value {
    if v.unsigned_abs() <= (1u64 << 53) {
        Value::int(v)
    } else {
        Value::number(v as f64)
    }
}

/// ES `Number::exponentiate` f64 path. The spec-mandated special cases are
/// applied here (in spec order: NaN exponent → NaN — `Math.pow(1, NaN)` is
/// NaN; ±0 exponent → 1 — `NaN ** 0` is 1; |base| == 1 with a ±Infinity
/// exponent → NaN; NaN base → NaN), then the general case falls to the
/// platform `powf`. The ES spec permits `**` results to be
/// "implementation-approximated", so a 1-ulp difference from V8 on rare
/// near-ties (`9 ** 17`: correctly-rounded 16677181699666568 vs V8's
/// 16677181699666570) is legal — the fuzz harness tolerates it.
#[inline]
fn js_powf(base: f64, exp: f64) -> f64 {
    if exp.is_nan() {
        return f64::NAN;
    }
    if exp == 0.0 {
        return 1.0; // +0 and -0
    }
    if exp.is_infinite() && base.abs() == 1.0 {
        return f64::NAN;
    }
    if base.is_nan() {
        return f64::NAN;
    }
    base.powf(exp)
}

/// ES Abstract Equality (`==`), §7.2.14, adapted to alloy's value model.
///
/// Steps 1–6 in order: same-type strict compare (int and f64 are one JS
/// "number" type), null↔undefined, number↔string via ToNumber, boolean via
/// ToNumber, object/array vs primitive via the default ToPrimitive (arrays
/// comma-join like `Array.prototype.toString`, plain objects become
/// "[object Object]"). User-defined `toString`/`valueOf` on objects is not
/// consulted — the Value layer is pure (no VM access), so the JS *default*
/// primitive is used instead. This is the one deliberate divergence.
fn abstract_eq(a: &Value, b: &Value) -> bool {
    // Step 1: same type → strict equality. Int and f64 are both JS numbers.
    let a_num = a.as_number().or_else(|| a.as_int().map(|i| i as f64));
    let b_num = b.as_number().or_else(|| b.as_int().map(|i| i as f64));
    if let (Some(x), Some(y)) = (a_num, b_num) {
        return x == y;
    }
    if a.is_undefined() && b.is_undefined() {
        return true;
    }
    if a.is_null() && b.is_null() {
        return true;
    }
    if let (Some(x), Some(y)) = (a.as_bool(), b.as_bool()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_str(), b.as_str()) {
        return x == y;
    }
    if a.is_object() && b.is_object() {
        return a.0 & PAYLOAD_MASK == b.0 & PAYLOAD_MASK;
    }
    if a.is_array() && b.is_array() {
        return a.0 & PAYLOAD_MASK == b.0 & PAYLOAD_MASK;
    }
    if a.is_function() && b.is_function() {
        return a.0 & PAYLOAD_MASK == b.0 & PAYLOAD_MASK;
    }
    // Natives are shared `Arc`s: identity is pointer equality (`Math.floor
    // === Math.floor` is true; two distinct natives never compare equal).
    if let (Some(x), Some(y)) = (a.as_native(), b.as_native()) {
        return Arc::ptr_eq(x, y);
    }
    if let (Some(x), Some(y)) = (a.as_promise(), b.as_promise()) {
        return Arc::ptr_eq(x, y);
    }
    // Step 2: null and undefined are equal to each other.
    if (a.is_null() || a.is_undefined()) && (b.is_null() || b.is_undefined()) {
        return true;
    }
    // Step 3: number vs string → compare with ToNumber(string).
    let a_num = a.as_number().or_else(|| a.as_int().map(|i| i as f64));
    let b_num = b.as_number().or_else(|| b.as_int().map(|i| i as f64));
    if let (Some(x), Some(s)) = (a_num, b.as_str()) {
        return x == js_string_to_number(s);
    }
    if let (Some(s), Some(y)) = (a.as_str(), b_num) {
        return js_string_to_number(s) == y;
    }
    // Step 4: boolean → ToNumber(bool) == other.
    if let Some(x) = a.as_bool() {
        return abstract_eq(&Value::number(if x { 1.0 } else { 0.0 }), b);
    }
    if let Some(y) = b.as_bool() {
        return abstract_eq(a, &Value::number(if y { 1.0 } else { 0.0 }));
    }
    // Step 5: object/array vs primitive → ToPrimitive (default path).
    if a.is_object() || a.is_array() {
        return abstract_eq(&to_primitive_default(a), b);
    }
    if b.is_object() || b.is_array() {
        return abstract_eq(a, &to_primitive_default(b));
    }
    // Step 6.
    false
}

/// JS ToString for a value: arrays comma-join their elements, objects become
/// "[object Object]", null → "null", undefined → "undefined", primitives
/// print raw (no quotes). Used by `join` separators and `String()`-style
/// coercion; join ELEMENTS apply the extra null/undefined → "" rule on top.
#[inline]
pub fn to_string_js(v: &Value) -> String {
    js_concat_str(v)
}

/// The JS string form of a value for the `+` operator and ToPrimitive
/// contexts: arrays comma-join their elements, objects become
/// "[object Object]", everything else is the raw primitive string.
fn js_concat_str(v: &Value) -> String {
    if v.is_array() || v.is_object() {
        match to_primitive_default(v).as_str() {
            Some(s) => s.to_string(),
            None => String::new(),
        }
    } else {
        to_string_raw(v)
    }
}

/// The default ToPrimitive for `==`: arrays comma-join their elements like
/// `Array.prototype.toString` (null/undefined become empty strings, nested
/// arrays recurse), plain objects become "[object Object]".
fn to_primitive_default(v: &Value) -> Value {
    if let Some(arr) = v.as_array() {
        let arr = arr.borrow();
        let mut parts: Vec<String> = Vec::with_capacity(arr.len());
        for e in arr.to_values() {
            if e.is_null() || e.is_undefined() {
                parts.push(String::new());
            } else if e.as_array().is_some() {
                parts.push(match to_primitive_default(&e).as_str() {
                    Some(s) => s.to_string(),
                    None => String::new(),
                });
            } else {
                parts.push(to_string_raw(&e));
            }
        }
        Value::string(parts.join(","))
    } else if v.is_object() {
        Value::string(object_js_string(v).unwrap_or_else(|| "[object Object]".to_string()))
    } else {
        v.clone()
    }
}

/// The JS string form of an object: `"Error: boom"` for error objects
/// (container 3), `[object Object]` for everything else. `None` means the
/// plain-object fallback should be used.
pub fn object_js_string(v: &Value) -> Option<String> {
    let od = v.as_object()?;
    let od = od.borrow();
    if od.container == DATE_CONTAINER {
        // Date: its `toString` shape ("Wed Jan 01 2024 …"), matching Node.
        return od
            .get(DATE_MS_KEY)
            .map(|x| date_to_string(x.to_number()));
    }
    if od.container != 3 {
        return None;
    }
    let get = |k: &str| -> String {
        od.get(k)
            .and_then(|x| x.as_str().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    let name = get("name");
    let msg = get("message");
    Some(if msg.is_empty() {
        name
    } else {
        format!("{}: {}", name, msg)
    })
}

// ---- Date ------------------------------------------------------------------
//
// Date instances are objects with `container = DATE_CONTAINER` storing the
// epoch milliseconds (a double, like JS) under the reserved key
// `DATE_MS_KEY` (a NUL-prefixed name no user code can collide with). The
// value-layer coercion hooks below read that key directly, so `+d`, `d - d`,
// `d < d2`, and `"" + d` behave like Node with no VM involvement — the one
// deliberate bridge between the pure Value layer and a container type.
//
// Time is the machine's local zone, resolved through the C runtime's
// localtime per instant (so DST shifts at different times of year come out
// right). There is no tz database: formatting uses generic `UTC±HH:MM` zone
// names instead of "Central European Standard Time".

/// Date-instance container tag (4 — 1/2/3 are Map/Set/Error).
pub const DATE_CONTAINER: u8 = 4;
/// Reserved own-property key holding a Date's epoch milliseconds.
pub const DATE_MS_KEY: &str = "\u{0}time";

/// The epoch-milliseconds stored in a Date instance, if `v` is one.
/// `Some(NaN)` means an Invalid Date; plain objects/arrays return `None`.
pub fn date_ms(v: &Value) -> Option<f64> {
    let od = v.as_object()?;
    let od = od.borrow();
    if od.container != DATE_CONTAINER {
        return None;
    }
    od.get(DATE_MS_KEY).map(|x| x.to_number())
}

/// Overwrite a Date instance's epoch-milliseconds (setters).
pub fn date_set_ms(v: &Value, ms: f64) {
    if let Some(od) = v.as_object() {
        let mut od = od.borrow_mut();
        if od.container == DATE_CONTAINER {
            od.set(DATE_MS_KEY, Value::number(ms));
        }
    }
}

/// Howard Hinnant's civil↔days algorithms: days-since-epoch (1970-01-01 = 0)
/// from a (proleptic Gregorian) year/month/day. Out-of-range month/day
/// overflow naturally (`d = 32` lands in the next month, `m = 13` in the
/// next year), which is exactly JS component semantics.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: days-since-epoch → (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The machine's UTC offset in milliseconds at the given epoch-ms instant,
/// resolved through the C runtime's localtime (per-instant, so DST is
/// right). 0 when unavailable (non-finite time or a platform without a
/// usable localtime).
///
/// The offset is computed without `mktime` (Windows' libc lacks it): the
/// local breakdown of `t` is inverted with `days_from_civil` — the epoch of
/// the local wall-clock components, read as UTC, is exactly `t + offset`.
pub fn local_offset_ms(utc_ms: f64) -> i64 {
    if !utc_ms.is_finite() {
        return 0;
    }
    let t = (utc_ms / 1000.0).round() as i64;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    {
        let r = unsafe { libc::localtime_r(&t, &mut tm) };
        if r.is_null() {
            return 0;
        }
    }
    #[cfg(not(unix))]
    {
        let r = unsafe { libc::localtime_s(&mut tm, &t) };
        if r != 0 {
            return 0;
        }
    }
    let local_epoch = days_from_civil(
        tm.tm_year as i64 + 1900,
        tm.tm_mon as i64 + 1,
        tm.tm_mday as i64,
    ) * 86_400
        + tm.tm_hour as i64 * 3600
        + tm.tm_min as i64 * 60
        + tm.tm_sec as i64;
    (local_epoch - t).saturating_mul(1000)
}

/// UTC breakdown of epoch ms: (year, month 1-12, day, hour, min, sec, ms).
/// NaN → NaN invalid (year = i64::MIN as the sentinel).
pub fn ms_components_utc(ms: f64) -> (i64, i64, i64, i64, i64, i64, i64) {
    if !ms.is_finite() {
        return (i64::MIN, 0, 0, 0, 0, 0, 0);
    }
    let total = ms.floor() as i64;
    let days = total.div_euclid(86_400_000);
    let rem = total.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let ms_part = rem.rem_euclid(1000);
    let secs = rem.div_euclid(1000);
    (
        y,
        m,
        d,
        secs.div_euclid(3600),
        secs.rem_euclid(3600).div_euclid(60),
        secs.rem_euclid(60),
        ms_part,
    )
}

/// Local breakdown of epoch ms: `ms_components_utc(ms + local offset)`.
pub fn ms_components_local(ms: f64) -> (i64, i64, i64, i64, i64, i64, i64) {
    ms_components_utc(ms + local_offset_ms(ms) as f64)
}

/// Epoch ms from (already-normalized, non-NaN) UTC components. Overflowing
/// month/day shift forward like JS. The caller applies the 1900+ rule for
/// 0-99 years and any NaN checks.
pub fn ms_from_utc_components(
    y: i64,
    mo: i64, // 0-based
    d: i64,
    h: i64,
    mi: i64,
    s: i64,
    ms: i64,
) -> f64 {
    let months_total = y * 12 + mo;
    let y = months_total.div_euclid(12);
    let m = months_total.rem_euclid(12) + 1;
    let days = days_from_civil(y, m, d);
    (days as f64 * 86_400_000.0)
        + (h as f64 * 3_600_000.0)
        + (mi as f64 * 60_000.0)
        + (s as f64 * 1000.0)
        + ms as f64
}

/// Epoch ms from LOCAL components: the local wall time minus the zone offset
/// at that wall time.
pub fn ms_from_local_components(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64) -> f64 {
    let wall = ms_from_utc_components(y, mo, d, h, mi, s, ms);
    wall - local_offset_ms(wall) as f64
}

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `GMT+0530` (or bare `GMT` at offset 0).
fn gmt_zone_str(offset_ms: i64) -> String {
    if offset_ms == 0 {
        return "GMT".to_string();
    }
    let sign = if offset_ms < 0 { "-" } else { "+" };
    let a = offset_ms.abs();
    format!("GMT{}{:02}{:02}", sign, a / 3_600_000, (a / 60_000) % 60)
}

/// `(UTC+05:30)`-style zone name (no tz database; generic names).
fn gmt_zone_paren(offset_ms: i64) -> String {
    if offset_ms == 0 {
        return "(Coordinated Universal Time)".to_string();
    }
    let sign = if offset_ms < 0 { "-" } else { "+" };
    let a = offset_ms.abs();
    format!("(UTC{}{:02}:{:02})", sign, a / 3_600_000, (a / 60_000) % 60)
}

/// Year with JS display padding: 4 digits for 0-9999, signed 6 otherwise.
fn pad_year(y: i64) -> String {
    if (0..=9999).contains(&y) {
        format!("{:04}", y)
    } else if y >= 0 {
        format!("+{:06}", y)
    } else {
        format!("-{:06}", -y)
    }
}

/// `Date.prototype.toString` shape: `Wed Jan 01 2024 12:00:00 GMT+0200 (UTC+02:00)`
/// (local time; "Invalid Date" for NaN).
pub fn date_to_string(ms: f64) -> String {
    if !ms.is_finite() {
        return "Invalid Date".to_string();
    }
    let (y, m, d, h, mi, s, _) = ms_components_local(ms);
    let days = (ms.floor() as i64).div_euclid(86_400_000);
    let off = local_offset_ms(ms);
    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} {} {}",
        DAY_NAMES[(days + 4).rem_euclid(7) as usize],
        MONTH_NAMES[(m - 1) as usize],
        d,
        pad_year(y),
        h,
        mi,
        s,
        gmt_zone_str(off),
        gmt_zone_paren(off)
    )
}

/// `2024-01-01T12:00:00.000Z` (UTC).
pub fn date_to_iso_string(ms: f64) -> String {
    let (y, m, d, h, mi, s, msp) = ms_components_utc(ms);
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        pad_year(y),
        m,
        d,
        h,
        mi,
        s,
        msp
    )
}

/// `Mon, 01 Jan 2024 12:00:00 GMT` (UTC).
pub fn date_to_utc_string(ms: f64) -> String {
    let (y, m, d, h, mi, s, _) = ms_components_utc(ms);
    let days = (ms.floor() as i64).div_euclid(86_400_000);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAY_NAMES[(days + 4).rem_euclid(7) as usize],
        d,
        MONTH_NAMES[(m - 1) as usize],
        pad_year(y),
        h,
        mi,
        s
    )
}

/// `Wed Jan 01 2024` (local date only).
pub fn date_to_date_string(ms: f64) -> String {
    let (y, m, d, _, _, _, _) = ms_components_local(ms);
    let days = (ms.floor() as i64).div_euclid(86_400_000);
    format!(
        "{} {} {:02} {}",
        DAY_NAMES[(days + 4).rem_euclid(7) as usize],
        MONTH_NAMES[(m - 1) as usize],
        d,
        pad_year(y)
    )
}

/// `12:00:00 GMT+0200 (UTC+02:00)` (local time of day).
pub fn date_to_time_string(ms: f64) -> String {
    let (_, _, _, h, mi, s, _) = ms_components_local(ms);
    let off = local_offset_ms(ms);
    format!(
        "{:02}:{:02}:{:02} {} {}",
        h,
        mi,
        s,
        gmt_zone_str(off),
        gmt_zone_paren(off)
    )
}

/// `1/1/2024` (local, Node's `toLocaleDateString` shape).
pub fn date_to_locale_date_string(ms: f64) -> String {
    let (y, m, d, _, _, _, _) = ms_components_local(ms);
    format!("{}/{}/{}", m, d, y)
}

/// `12:00:00 PM` (local, Node's `toLocaleTimeString` shape, 12-hour).
pub fn date_to_locale_time_string(ms: f64) -> String {
    let (_, _, _, h, mi, s, _) = ms_components_local(ms);
    let (h12, ampm) = match h.rem_euclid(24) {
        0 => (12, "am"),
        12 => (12, "pm"),
        h if h < 12 => (h, "am"),
        h => (h - 12, "pm"),
    };
    format!("{:02}:{:02}:{:02} {}", h12, mi, s, ampm)
}

/// `Date.parse`: ISO 8601 (`YYYY-MM-DD[THH:mm[:ss[.sss]][Z|±HH:mm]]`) plus a
/// few common fallbacks (`M/D/YYYY`, `YYYY/M/D`). Date-only forms are UTC
/// midnight; time-without-zone forms are local. Unparseable → NaN.
pub fn date_parse(s: &str) -> f64 {
    let input = s.trim();
    if input.is_empty() {
        return f64::NAN;
    }
    let b = input.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let digits = |b: &[u8], i: &mut usize, max: usize| -> Option<i64> {
        let start = *i;
        let mut v: i64 = 0;
        while *i < b.len() && b[*i].is_ascii_digit() && *i - start < max {
            v = v * 10 + (b[*i] - b'0') as i64;
            *i += 1;
        }
        if *i == start {
            None
        } else {
            Some(v)
        }
    };
    // [±]YYYY (2-6 digits; a bare 2-digit year gets the 1900 rule).
    let mut sign = 1i64;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        if b[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let Some(year) = digits(b, &mut i, 6) else {
        return f64::NAN;
    };
    let mut y = sign * year;
    let mut mo: Option<i64> = None;
    let mut d: Option<i64> = None;
    let mut h = 0i64;
    let mut mi = 0i64;
    let mut s = 0i64;
    let mut ms = 0i64;
    let mut has_time = false;
    let mut tz_min: Option<i64> = None; // minutes EAST of UTC for the string
    if i < n && b[i] == b'-' {
        i += 1;
        mo = digits(b, &mut i, 2);
        if i < n && b[i] == b'-' {
            i += 1;
            d = digits(b, &mut i, 2);
        }
    }
    if i < n && (b[i] == b'T' || b[i] == b't' || b[i] == b' ') {
        has_time = true;
        i += 1;
        h = digits(b, &mut i, 2).unwrap_or(0);
        if i < n && b[i] == b':' {
            i += 1;
            mi = digits(b, &mut i, 2).unwrap_or(0);
            if i < n && b[i] == b':' {
                i += 1;
                s = digits(b, &mut i, 2).unwrap_or(0);
                if i < n && b[i] == b'.' {
                    i += 1;
                    let start = i;
                    let frac = digits(b, &mut i, 9).unwrap_or(0);
                    let len = i - start;
                    ms = if len >= 3 { frac % 1000 } else { frac * 10i64.pow((3 - len) as u32) };
                }
            }
        }
    }
    if i < n && (b[i] == b'Z' || b[i] == b'z') {
        tz_min = Some(0);
        i += 1;
    } else if i < n && (b[i] == b'+' || b[i] == b'-') {
        let neg = b[i] == b'-';
        i += 1;
        let oh = digits(b, &mut i, 2).unwrap_or(0);
        let mut om = 0i64;
        if i < n && b[i] == b':' {
            i += 1;
            om = digits(b, &mut i, 2).unwrap_or(0);
        } else if i < n && i + 1 < n && b[i].is_ascii_digit() && b[i + 1].is_ascii_digit() {
            om = digits(b, &mut i, 2).unwrap_or(0);
        }
        let total = oh * 60 + om;
        tz_min = Some(if neg { -total } else { total });
    }
    if i != n {
        return f64::NAN;
    }
    // Component validation.
    if let Some(mo) = mo {
        if !(1..=12).contains(&mo) {
            return f64::NAN;
        }
    }
    if let Some(d) = d {
        if !(1..=31).contains(&d) {
            return f64::NAN;
        }
    }
    if !(0..=24).contains(&h) || !(0..=59).contains(&mi) || !(0..=59).contains(&s) {
        return f64::NAN;
    }
    // The 1900 rule applies only to a bare 2-digit year (no century digits
    // were consumed). We consumed at most 6 digits; re-derive the count.
    {
        let mut i2 = 0usize;
        if !input.is_empty() && (b[i2] == b'+' || b[i2] == b'-') {
            i2 += 1;
        }
        let ystart = i2;
        while i2 < n && b[i2].is_ascii_digit() {
            i2 += 1;
        }
        let ylen = i2 - ystart;
        if ylen == 2 {
            y = 1900 + sign * year;
        }
    }
    let days = days_from_civil(y, mo.unwrap_or(1), d.unwrap_or(1));
    let utc = days as f64 * 86_400_000.0
        + h as f64 * 3_600_000.0
        + mi as f64 * 60_000.0
        + s as f64 * 1000.0
        + ms as f64;
    match tz_min {
        Some(m) => utc - m as f64 * 60_000.0,
        None if has_time => {
            // Time without zone → local wall time.
            utc - local_offset_ms(utc) as f64
        }
        None => utc, // date-only → UTC midnight
    }
}

/// ES ToInt32 (§7.1.6): NaN/±Infinity → 0, then truncate toward zero and
/// fold into the signed 32-bit range. The f64 mod-2^32 keeps huge magnitudes
/// (2^31 and beyond) correct without overflowing i64.
#[inline]
fn to_int32_value(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() {
        0
    } else {
        let t = n.trunc();
        let m = t % 4294967296.0;
        let m = if m < 0.0 { m + 4294967296.0 } else { m };
        m as u32 as i32
    }
}

/// JS ToNumber for strings (§7.1.3.1): trim whitespace, handle "",
/// "Infinity", hex/octal/binary prefixes, and decimal forms Rust's `parse`
/// rejects (".5", "5.", "1.e3"). Anything unparseable → NaN.
pub fn js_string_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    let (neg, rest) = match t.as_bytes()[0] {
        b'+' => (false, &t[1..]),
        b'-' => (true, &t[1..]),
        _ => (false, t),
    };
    if rest == "Infinity" {
        return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let radix_fold = |rest: &str, radix: u32| -> f64 {
        let mut acc = 0.0f64;
        for c in rest.chars() {
            match c.to_digit(radix) {
                Some(d) => acc = acc * radix as f64 + d as f64,
                None => return f64::NAN,
            }
        }
        if neg { -acc } else { acc }
    };
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        return radix_fold(hex, 16);
    }
    if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        return radix_fold(bin, 2);
    }
    if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        return radix_fold(oct, 8);
    }
    // Decimal: normalize the mantissa so Rust's parser accepts "5." and ".5"
    // (and their exponent forms "5.e3" / ".5e3").
    let (mant, exp) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let mant = if mant.starts_with('.') {
        format!("0{}", mant)
    } else if mant.ends_with('.') {
        format!("{}0", mant)
    } else {
        mant.to_string()
    };
    let norm = match exp {
        Some(e) => format!("{}e{}", mant, e),
        None => mant,
    };
    let n = norm.parse::<f64>().unwrap_or(f64::NAN);
    if neg { -n } else { n }
}

/// Coerce a value to its raw string form (strings unquoted), for JS-style
/// string concatenation.
fn to_string_raw(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else if v.is_undefined() {
        "undefined".to_string()
    } else if v.is_null() {
        "null".to_string()
    } else if let Some(b) = v.as_bool() {
        b.to_string()
    } else if let Some(n) = v.as_number() {
        num_to_string(n)
    } else if let Some(i) = v.as_int() {
        i.to_string()
    } else {
        format!("{}", v)
    }
}

/// JS `Number.prototype.toString` (radix 10) / `String(number)`: shortest
/// round-trip digits with V8's exponential thresholds — `|n| >= 1e21` or
/// (`0 < |n| < 1e-6`) print in exponent form (`"1e+21"`, `"1.5e-7"`), while
/// Rust's `Display` prints the full fixed string. The mantissa digits match
/// (both shortest-round-trip); only the notation differs.
pub fn js_number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if n == 0.0 {
        return "0".to_string();
    }
    let a = n.abs();
    if a >= 1e21 || a < 1e-6 {
        // Rust's {:e} gives the same shortest mantissa as V8; only the
        // exponent notation differs ("1e21" vs "1e+21", "1.5e-7" vs "1.5e-7").
        let s = format!("{:e}", n);
        let (mant, exp) = s.split_once('e').expect("format e always has an exponent");
        let e: i64 = exp.parse().expect("exponent is numeric");
        format!("{}e{}{}", mant, if e < 0 { "-" } else { "+" }, e.abs())
    } else {
        num_to_string(n)
    }
}

/// JS ToString for a number. Integral values up to 2^53 print as plain
/// integers; everything else (including integral doubles beyond 2^53) prints
/// as the shortest round-trip decimal — matching V8, which never prints the
/// full integer for a double it can't represent exactly. E.g. the double
/// 110860087999499552 prints as "110860087999499550" in both engines, while
/// the old `n == (n as i64) as f64` fast path wrongly printed the full
/// integer. (Values >= 1e21 that V8 prints in exponent form are formatted
/// elsewhere — see the VM's number formatting.)
fn num_to_string(n: f64) -> String {
    if n.is_infinite() {
        if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if n == n.trunc() && n.abs() <= 9007199254740992.0 {
        (n as i64).to_string()
    } else {
        n.to_string()
    }
}

/// JS-style number stringification (shared by Display and JSON.stringify):
/// `-0` → "0", NaN/±Infinity → "null" (the JSON mapping), finite values via
/// the engine's shortest representation.
#[inline]
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() || n.is_infinite() {
        "null".to_string()
    } else if n == 0.0 && n.is_sign_negative() {
        "0".to_string()
    } else {
        num_to_string(n)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_undefined() {
            write!(f, "undefined")
        } else if self.is_null() {
            write!(f, "null")
        } else if let Some(b) = self.as_bool() {
            write!(f, "{}", b)
        } else if let Some(n) = self.as_number() {
            // console.log-style display: V8 prints `-0` for negative zero
            // (JS *string* coercion — `String(-0)`, `"" + -0`, array join —
            // prints "0", handled by `to_string_raw` above).
            if n == 0.0 && n.is_sign_negative() {
                write!(f, "-0")
            } else {
                write!(f, "{}", num_to_string(n))
            }
        } else if let Some(i) = self.as_int() {
            write!(f, "{}", i)
        } else if let Some(s) = self.as_str() {
            write!(f, "\"{}\"", s)
        } else if let Some(arr) = self.as_array() {
            let arr = arr.borrow();
            write!(f, "[")?;
            for (i, v) in arr.to_values().iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", v)?;
            }
            write!(f, "]")
        } else if self.is_object() {
            match object_js_string(self) {
                Some(s) => write!(f, "{}", s),
                None => write!(f, "[object Object]"),
            }
        } else if let Some(fd) = self.as_function() {
            write!(f, "[function p{} @{}]", fd.program, fd.ptr)
        } else if self.is_native() {
            write!(f, "[native function]")
        } else if self.0 & TAG_MASK == TAG_SMALL && self.0 & 0xF == S_SYMBOL {
            write!(f, "Symbol({})", self.0 >> 4)
        } else if let Some(p) = self.as_pointer() {
            write!(f, "Pointer({:p})", p)
        } else if let Some((ptr, len)) = self.as_buffer() {
            write!(f, "SharedBuffer({:p}, {})", ptr, len)
        } else if let Some(c) = self.as_cell() {
            let inner = c.borrow();
            write!(f, "<cell {}>", inner)
        } else if self.is_promise() {
            write!(f, "Promise")
        } else if let Some(r) = self.as_regex() {
            // console.log-style display: `/pattern/flags`.
            let g = r.lock().unwrap_or_else(|g| g.into_inner());
            write!(f, "{}", g.compiled.to_source_string())
        } else {
            write!(f, "Value")
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_undefined() {
            write!(f, "Undefined")
        } else if self.is_null() {
            write!(f, "Null")
        } else if let Some(b) = self.as_bool() {
            write!(f, "Bool({})", b)
        } else if let Some(n) = self.as_number() {
            write!(f, "Number({})", n)
        } else if let Some(i) = self.as_int() {
            write!(f, "Int({})", i)
        } else if let Some(s) = self.as_str() {
            write!(f, "String({:?})", s)
        } else if let Some(a) = self.as_array() {
            write!(f, "Array({:?})", a.borrow())
        } else if let Some(m) = self.as_object() {
            write!(f, "Object({:?})", m.borrow())
        } else if let Some(fd) = self.as_function() {
            write!(f, "Function(p{} @{}, {} upvalues)", fd.program, fd.ptr, fd.cells.len())
        } else if self.is_native() {
            write!(f, "NativeFunction")
        } else if let Some(p) = self.as_pointer() {
            write!(f, "Pointer({:p})", p)
        } else if let Some((ptr, len)) = self.as_buffer() {
            write!(f, "SharedBuffer({:p}, {})", ptr, len)
        } else if self.is_cell() {
            write!(f, "Cell")
        } else if let Some(p) = self.as_promise() {
            let st = p.lock().unwrap_or_else(|g| g.into_inner());
            write!(f, "Promise({:?}, {} conts)", st.status, st.continuations.len())
        } else {
            write!(f, "Value")
        }
    }
}

unsafe impl Send for Value {}
unsafe impl Sync for Value {}

// ---- escape analysis (generational arena) ----------------------------------
//
// At a unit boundary (end of a script run, end of an HTTP request) the VM
// walks every value reachable from its persistent roots: young boxes are
// copied into the old generation (fixing up interior references recursively),
// the contents of the dead young boxes are dropped (releasing the `Rc`s they
// held), and the young generation is bulk-reset. Old boxes are never traced
// inline — the write barrier's dirty-box scan promotes any young values
// written into them — so the per-unit cost is O(young + dirty), not O(live).
//
// The second-generation sweep is INCREMENTAL. When the old gen churns past
// the threshold, a [`MarkState`] spans unit boundaries: each slice records
// newly-reached old boxes (via `walk_value`'s `mark` and the dirty-box scan),
// traces a bounded budget of queued boxes, and re-traces whatever the write
// barriers dirtied (closure cells, promises, channels). When the queue
// drains, `sweep_old_mark_sweep` runs — dropping the un-marked old boxes and
// coalescing their space onto the free list. Live boxes are NEVER copied or
// moved, and no single unit does more than the budget's worth of marking, so
// a huge live graph never stalls a request.
//
// `map` dedups young boxes (young addr → old addr); `visited` bounds the
// `Rc`-backed containers (cells, functions, channels, promises), whose `Rc`
// cycles (mutually-recursive closures) would otherwise re-enter forever.

use crate::heap::{ArenaHeap, PromoteMap, KIND_ARRAY, KIND_OBJECT, KIND_STRING};
use std::collections::HashSet;

/// An Rc-backed structure the incremental-GC write barrier dirtied. The ref
/// is held strongly so the structure stays alive until the next mark slice
/// re-traces it (a raw address could dangle if the last owner dropped it).
#[derive(Clone)]
pub enum RcDirtyRef {
    Cell(Rc<RefCell<Value>>),
    Promise(Arc<Mutex<PromiseState>>),
    Channel(Arc<Mutex<ChannelState>>),
}

/// Persistent state of the incremental major-GC mark. Spans unit boundaries:
/// each slice marks a bounded number of worklist boxes plus whatever the
/// write barriers dirtied, so a huge live graph never stalls a single
/// request. When the worklist and the dirty set both drain, the sweep runs.
#[derive(Default)]
pub struct MarkState {
    /// Marked old box payload addresses (and, for strings, their byte
    /// regions) — the sweep keeps exactly these.
    pub set: HashSet<usize>,
    /// Old boxes whose interiors still need tracing (budgeted per slice).
    pub worklist: Vec<usize>,
    /// Rc-backed structures mutated since the last slice.
    pub dirty_rc: Vec<RcDirtyRef>,
}

impl MarkState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mark is complete: nothing left to trace.
    pub fn is_done(&self) -> bool {
        self.worklist.is_empty() && self.dirty_rc.is_empty()
    }

    /// Record a box as live and queue its interior for tracing (strings also
    /// mark their byte regions — for ropes, the whole subtree). Returns true
    /// if newly marked.
    pub fn mark_box(&mut self, heap: &ArenaHeap, addr: usize) -> bool {
        if self.set.insert(addr) {
            self.worklist.push(addr);
            if heap.addr_in_old(addr) && heap.kind_of(addr) == KIND_STRING as u8 {
                mark_string_subtree(heap, addr, &mut self.set);
            }
            true
        } else {
            false
        }
    }

    /// Record a box as live without queuing it (promoted boxes are already
    /// fully traced by the promote walk; the sweep just needs to know them).
    pub fn insert_box(&mut self, heap: &ArenaHeap, addr: usize) {
        if self.set.insert(addr) && heap.addr_in_old(addr) && heap.kind_of(addr) == KIND_STRING as u8 {
            mark_string_subtree(heap, addr, &mut self.set);
        }
    }
}

/// Mark every box and byte region of a (possibly rope) string subtree —
/// iteratively, so deep left-leaning ropes built by `s += t` can't overflow
/// the stack. Only addresses inside the swept old generation are recorded;
/// foreign (program/thread heap) children are immortal and skipped. The
/// caller has already inserted the root box.
fn mark_string_subtree(heap: &ArenaHeap, root: usize, set: &mut HashSet<usize>) {
    let mut stack = vec![root];
    while let Some(a) = stack.pop() {
        let b = unsafe { &*(a as *const AString) };
        if b.is_cons() {
            let left = b.left();
            let right = b.right();
            for c in [payload_to_usize(left.0), payload_to_usize(right.0)] {
                if heap.addr_in_old(c) && set.insert(c) {
                    stack.push(c);
                }
            }
        } else {
            // Builder boxes keep their buffer region alive; flat strings
            // keep their byte region.
            set.insert(b.contiguous_bytes() as usize);
        }
    }
}

/// Promote one string box's payload after the box itself was copied: cons
/// nodes queue their children; flat boxes repoint their byte region; builder
/// boxes repoint their buffer (marker and capacity are copied as-is).
fn promote_string_payload(heap: &mut ArenaHeap, copy: *mut AString, stack: &mut Vec<usize>) {
    if unsafe { (*copy).is_cons() } {
        let lv = unsafe { (*copy).left() };
        let rv = unsafe { (*copy).right() };
        stack.push(payload_to_usize(lv.0));
        stack.push(payload_to_usize(rv.0));
    } else if unsafe { (*copy).is_builder() } {
        let len = unsafe { (*copy).builder_len() };
        let old_bytes = unsafe { (*copy).builder_bytes() };
        let new_bytes = heap.promote_bytes(old_bytes, len);
        unsafe { (*copy).set_builder_bytes(new_bytes); }
    } else {
        let len = unsafe { (*copy).len };
        let old_bytes = unsafe { (*copy).bytes };
        unsafe { (*copy).bytes = heap.promote_bytes(old_bytes, len); }
    }
}

/// Walk a closure cell's content, deduping on the cell's address so the same
/// cell is never borrowed twice in one walk (mutually-recursive closures
/// share cells through their upvalue lists).
#[inline]
pub fn walk_cell(
    heap: &mut ArenaHeap,
    map: &mut PromoteMap,
    visited: &mut HashSet<usize>,
    mut mark: Option<&mut MarkState>,
    cell: &Rc<RefCell<Value>>,
) {
    let addr = Rc::as_ptr(cell) as usize;
    if visited.insert(addr) {
        walk_value(heap, map, visited, mark.as_deref_mut(), &mut cell.borrow_mut());
    }
}

/// Walk one reachable value: young boxes are copied into the old generation
/// (fixing up their interiors recursively); old boxes are never traced here —
/// young values written into them are handled by the VM's dirty-box scan —
/// but when `mark` is active they are recorded for the incremental mark's
/// budgeted worklist trace. Rc-backed structures (cells, functions, promises,
/// channels) are traversed with a `visited` set so cycles terminate.

/// A Map/Set entry key: wraps a [`Value`] with SameValueZero equality and
/// content hashing, so the engine's `HashMap` can host JS keys — `1` and
/// `1.0` share a slot, `NaN` keys work, `-0`/`+0` collide, and two strings
/// with equal content are the same key even across rope boxes.
#[derive(Debug, Clone)]
pub struct MapKey(pub Value);

impl PartialEq for MapKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.same_value_zero(&other.0)
    }
}
impl Eq for MapKey {}
impl std::hash::Hash for MapKey {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.hash_key());
    }
}

/// Arena-backed open-addressing hash slot: 24 bytes (`u64` hash + two
/// NaN-boxed `Value`s). `hash == 0` marks an empty slot, `hash == 1` a
/// tombstone (deleted); stored hashes are always `hash_key() | 2`, so the
/// two markers can never collide with a real key. The slot array itself is a
/// `KIND_RAW` region in the arena — a Map/Set's hash storage is literal
/// arena memory, bump-allocated and reclaimed by the sweeps like string
/// bytes.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct HashSlot {
    pub hash: u64,
    pub key: Value,
    pub val: Value,
    /// Position in `ContainerData::order` — lets `remove` tombstone the
    /// exact order slot, so a deleted key can never resurrect when the same
    /// key is re-added (Node moves it to the end).
    pub idx: usize,
}

const CONTAINER_LOAD: usize = 70; // percent; rehash when (live + deleted) passes it
const CONTAINER_INIT_CAP: usize = 8;

/// Murmur3-style 64-bit finalizer: avalanche all input bits into the low
/// bits. `hash_key` feeds it the per-type raw hash — without this, the f64
/// bit pattern of an integer key has its low 48 bits zero and every int key
/// collides in the table's low bits (linear probing degenerates to O(n)).
#[inline]
fn mix_hash(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// Allocate a zeroed slot array for `cap` slots from the current (young)
/// heap. A container whose box is already old repairs the generation
/// mismatch at the next collection: its write-barrier bit flags the box, and
/// `walk_container_entries` moves the region to the old gen before the young
/// arena is reset.
fn alloc_slots(cap: usize) -> (*mut HashSlot, usize) {
    let heap = heap::current_heap();
    let size = cap * std::mem::size_of::<HashSlot>();
    let p = unsafe { (*heap).alloc_raw_region(size, KIND_RAW) } as *mut HashSlot;
    unsafe {
        std::ptr::write_bytes(p as *mut u8, 0, size);
    }
    (p, cap)
}

/// Map/Set entries: an arena-backed open-addressing table (the hash
/// storage) plus the insertion-order key sequence that powers
/// `keys()`/`values()`/`entries()`/`forEach` (Node order semantics — the
/// engine has no iterator protocol, so those return array snapshots).
/// Probes are linear with a power-of-two capacity; deletes tombstone a slot
/// (hash = 1) and lazily rehash, and the order list is compacted once dead
/// keys outnumber live ones 2:1.
#[derive(Debug)]
pub struct ContainerData {
    /// Arena region holding `cap` slots (power of two).
    pub slots: *mut HashSlot,
    /// Number of slots in the region.
    pub cap: usize,
    /// Live entries (excluding tombstones).
    pub used: usize,
    /// Tombstoned slots; a rehash clears them (they slow probing).
    pub table_tombs: usize,
    /// Insertion order of every key ever added; deleted keys stay until
    /// compaction (iteration re-checks table membership to skip them).
    pub order: Vec<Option<MapKey>>,
}

impl Default for ContainerData {
    fn default() -> Self {
        let (slots, cap) = alloc_slots(CONTAINER_INIT_CAP);
        ContainerData {
            slots,
            cap,
            used: 0,
            table_tombs: 0,
            order: Vec::new(),
        }
    }
}

impl ContainerData {
    /// Number of live entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.used
    }

    /// Index of the slot holding `k`, or None. Linear probe from the key's
    /// bucket; stops at an empty slot (a tombstone is not a stop — the key
    /// may sit beyond it).
    #[inline]
    fn probe(&self, k: &Value) -> Option<usize> {
        let h = k.hash_key() | 2;
        let mask = self.cap - 1;
        let mut i = (h as usize) & mask;
        loop {
            let slot = unsafe { &*self.slots.add(i) };
            if slot.hash == 0 {
                return None;
            }
            if slot.hash == h && slot.key.same_value_zero(k) {
                return Some(i);
            }
            i = (i + 1) & mask;
        }
    }

    #[inline]
    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.probe(k).map(|i| unsafe { &(*self.slots.add(i)).val })
    }

    #[inline]
    pub fn contains(&self, k: &Value) -> bool {
        self.probe(k).is_some()
    }

    /// Insert or update `k → v`; a fresh key appends to the order list. An
    /// update keeps the key's position (Node semantics); delete+re-add
    /// appends at the end.
    pub fn insert(&mut self, k: Value, v: Value) {
        if (self.used + self.table_tombs + 1) * 100 > self.cap * CONTAINER_LOAD {
            // Grow (doubling) — or clear tombstones if capacity suffices.
            let grow = (self.used + 1) * 100 > self.cap * CONTAINER_LOAD;
            self.rehash(if grow { self.cap * 2 } else { self.cap });
        }
        let h = k.hash_key() | 2;
        let mask = self.cap - 1;
        let mut i = (h as usize) & mask;
        let mut first_tomb: Option<usize> = None;
        loop {
            let slot = unsafe { &*self.slots.add(i) };
            if slot.hash == h && slot.key.same_value_zero(&k) {
                unsafe { (*self.slots.add(i)).val = v; }
                return;
            }
            if slot.hash == 0 {
                let place = first_tomb.unwrap_or(i);
                let s = unsafe { &mut *self.slots.add(place) };
                if s.hash == 1 {
                    self.table_tombs -= 1;
                }
                s.hash = h;
                s.key = k.clone();
                s.val = v;
                s.idx = self.order.len();
                self.used += 1;
                self.order.push(Some(MapKey(k)));
                return;
            }
            if slot.hash == 1 && first_tomb.is_none() {
                first_tomb = Some(i);
            }
            i = (i + 1) & mask;
        }
    }

    /// Delete `k`; returns whether it was present. The slot is tombstoned
    /// (its key/value references released), and both the table and the order
    /// list compact lazily when dead entries dominate.
    pub fn remove(&mut self, k: &Value) -> bool {
        let Some(i) = self.probe(k) else {
            return false;
        };
        let slot = unsafe { &mut *self.slots.add(i) };
        let idx = slot.idx;
        slot.hash = 1;
        slot.key = Value::undefined();
        slot.val = Value::undefined();
        // Tombstone the exact order slot (releasing the key reference); a
        // re-add appends a fresh slot, so the key moves to the end.
        if idx < self.order.len() {
            self.order[idx] = None;
        }
        self.used -= 1;
        self.table_tombs += 1;
        if (self.used + self.table_tombs) * 100 > self.cap * CONTAINER_LOAD {
            self.rehash(self.cap); // clear tombstones, keep capacity
        }
        if self.order.len() > self.used * 2 + 8 {
            self.compact_order();
        }
        true
    }

    /// Re-insert every live slot into a fresh zeroed region of `new_cap`
    /// slots (allocated from the current heap). The old region is left
    /// unreachable and reclaimed by the next sweep — nothing marks it.
    fn rehash(&mut self, new_cap: usize) {
        let (new_slots, _) = alloc_slots(new_cap);
        let mask = new_cap - 1;
        let mut used = 0usize;
        for i in 0..self.cap {
            let s = unsafe { &*self.slots.add(i) };
            if s.hash >= 2 {
                let mut j = (s.hash as usize) & mask;
                loop {
                    let t = unsafe { &mut *new_slots.add(j) };
                    if t.hash == 0 {
                        *t = s.clone();
                        used += 1;
                        break;
                    }
                    j = (j + 1) & mask;
                }
            }
        }
        self.slots = new_slots;
        self.cap = new_cap;
        self.used = used;
        self.table_tombs = 0;
    }

    /// Drop every entry, keeping the (now empty) region.
    pub fn clear(&mut self) {
        let size = self.cap * std::mem::size_of::<HashSlot>();
        unsafe {
            std::ptr::write_bytes(self.slots as *mut u8, 0, size);
        }
        self.used = 0;
        self.table_tombs = 0;
        self.order.clear();
    }

    /// Live `(key, value)` pairs in insertion order (values are the keys
    /// themselves for Sets). Each step re-checks table membership, so
    /// entries deleted mid-walk are skipped and appended entries are seen if
    /// they land before the walk finishes — Node's spec latitude.
    pub fn iter(&self) -> impl Iterator<Item = (Value, Value)> + '_ {
        self.order.iter().filter_map(move |slot| {
            let k = slot.as_ref()?;
            let i = self.probe(&k.0)?;
            let s = unsafe { &*self.slots.add(i) };
            Some((k.0.clone(), s.val.clone()))
        })
    }

    /// Length of the order list (including dead keys) — the live-walk bound.
    #[inline]
    pub fn order_len(&self) -> usize {
        self.order.len()
    }

    /// The live pair at order position `i`, or None if it was deleted (or
    /// the position is past the end). Used by the `forEach` live walk.
    #[inline]
    pub fn order_pair(&self, i: usize) -> Option<(Value, Value)> {
        let k = self.order.get(i)?.as_ref()?;
        let j = self.probe(&k.0)?;
        let s = unsafe { &*self.slots.add(j) };
        Some((k.0.clone(), s.val.clone()))
    }

    /// Drop dead order slots (already `None` — `remove` tombstones the exact
    /// slot via its stored index), preserving the survivors' relative order.
    /// Amortized O(1) per delete.
    fn compact_order(&mut self) {
        let mut order = Vec::with_capacity(self.used);
        for slot in self.order.iter() {
            if let Some(k) = slot {
                order.push(Some(k.clone()));
            }
        }
        self.order = order;
    }
}

/// GC-walk a Map/Set's entries, shared by the promote walk, the dirty-box
/// scan, and the incremental-mark trace (all three can move or promote
/// young keys):
///
/// * The order list holds keys only (it never hashes) — young keys are
///   remapped in place.
/// * The table's slots are walked in place; an identity-hashed key (object,
///   array, …) whose address was remapped invalidates its stored hash, and
///   a slot region still in the young generation dies with the young arena's
///   bulk reset — either way the table is rebuilt into a fresh *old*
///   generation region with recomputed hashes.
/// * The live slot region is recorded into the mark set (like string byte
///   regions), so the non-moving old sweep keeps it.
pub fn walk_container_entries(
    cd: &mut ContainerData,
    heap: &mut ArenaHeap,
    map: &mut PromoteMap,
    visited: &mut HashSet<usize>,
    mut mark: Option<&mut MarkState>,
) {
    for slot in cd.order.iter_mut() {
        if let Some(k) = slot.as_mut() {
            walk_value(heap, map, visited, mark.as_deref_mut(), &mut k.0);
        }
    }
    let mut stale = false;
    for i in 0..cd.cap {
        let slot = unsafe { &mut *cd.slots.add(i) };
        if slot.hash >= 2 {
            walk_value(heap, map, visited, mark.as_deref_mut(), &mut slot.key);
            walk_value(heap, map, visited, mark.as_deref_mut(), &mut slot.val);
            // Only identity-hashed keys (address-based) can go stale; string
            // and numeric keys hash by content/value, so their stored hashes
            // survive a remap untouched.
            match slot.key.bits() & TAG_MASK {
                TAG_OBJ | TAG_ARR | TAG_FN | TAG_CELL | TAG_MISC => {
                    if (slot.key.hash_key() | 2) != slot.hash {
                        stale = true;
                    }
                }
                _ => {}
            }
        }
    }
    if heap.addr_in_young(cd.slots as usize) || stale {
        let size = cd.cap * std::mem::size_of::<HashSlot>();
        let new_slots = heap.alloc_old_bytes_uninit(size) as *mut HashSlot;
        unsafe {
            std::ptr::write_bytes(new_slots as *mut u8, 0, size);
        }
        let mask = cd.cap - 1;
        let mut used = 0usize;
        for i in 0..cd.cap {
            let s = unsafe { &*cd.slots.add(i) };
            if s.hash >= 2 {
                let h = s.key.hash_key() | 2;
                let mut j = (h as usize) & mask;
                loop {
                    let t = unsafe { &mut *new_slots.add(j) };
                    if t.hash == 0 {
                        *t = HashSlot {
                            hash: h,
                            key: s.key.clone(),
                            val: s.val.clone(),
                            idx: s.idx,
                        };
                        used += 1;
                        break;
                    }
                    j = (j + 1) & mask;
                }
            }
        }
        cd.slots = new_slots;
        cd.used = used;
        cd.table_tombs = 0;
    }
    // The sweep keeps exactly what the mark set records: a live container's
    // slot region must be in it, or the non-moving sweep frees it from under
    // the container.
    if let Some(m) = mark.as_deref_mut() {
        m.set.insert(cd.slots as usize);
    }
}
pub fn walk_value(
    heap: &mut ArenaHeap,
    map: &mut PromoteMap,
    visited: &mut HashSet<usize>,
    mut mark: Option<&mut MarkState>,
    v: &mut Value,
) {
    let tag = v.0 & TAG_MASK;
    match tag {
        TAG_STR | TAG_ARR | TAG_OBJ => {
            let addr = payload_to_usize(v.0);
            if heap.addr_in_young(addr) {
                if let Some(&new_addr) = map.get(&addr) {
                    *v = Value(tag | usize_to_payload(new_addr));
                    return;
                }
                let new_addr = heap.promote_box(addr);
                // Insert before recursing so cycles terminate.
                map.insert(addr, new_addr);
                match tag {
                    TAG_STR => {
                        // Promote the whole rope subtree without recursion (a
                        // left-leaning rope built by `s += t` can be deep).
                        // The root box is already copied; pass 1 promotes
                        // every descendant box and repoints flat byte
                        // regions, pass 2 rewires cons children using the
                        // map. The young originals stay intact until the
                        // sweep, so pass 2 reads them for child addresses.
                        let mut rope_stack: Vec<usize> = Vec::new();
                        {
                            let copy = new_addr as *mut AString;
                            promote_string_payload(heap, copy, &mut rope_stack);
                        }
                        while let Some(a) = rope_stack.pop() {
                            if !heap.addr_in_young(a) || map.contains_key(&a) {
                                continue;
                            }
                            let na = heap.promote_box(a);
                            map.insert(a, na);
                            let copy = na as *mut AString;
                            promote_string_payload(heap, copy, &mut rope_stack);
                        }
                        // Pass 2: rewire cons children to the promoted
                        // addresses (children outside this heap — program or
                        // thread constants — are stable and left alone).
                        let mut fix: Vec<usize> = vec![addr];
                        while let Some(a) = fix.pop() {
                            let new_a = match map.get(&a) {
                                Some(&n) => n,
                                None => continue,
                            };
                            let copy = new_a as *mut AString;
                            if !unsafe { (*copy).is_cons() } {
                                continue;
                            }
                            // The young original still holds the
                            // pre-promotion child addresses.
                            let orig = a as *const AString;
                            let left = unsafe { Value((*orig).bytes as u64) };
                            let right = unsafe { Value((*orig).len as u64) };
                            let (la, ra) = (payload_to_usize(left.0), payload_to_usize(right.0));
                            if let Some(&nl) = map.get(&la) {
                                unsafe { (*copy).bytes = (TAG_STR | usize_to_payload(nl)) as *mut u8; }
                            }
                            if let Some(&nr) = map.get(&ra) {
                                unsafe { (*copy).len = (TAG_STR | usize_to_payload(nr)) as usize; }
                            }
                            if heap.addr_in_young(la) {
                                fix.push(la);
                            }
                            if heap.addr_in_young(ra) {
                                fix.push(ra);
                            }
                        }
                    }
                    TAG_ARR => {
                        let copy = new_addr as *const RefCell<ArrayData>;
                        let mut inner = unsafe { &*copy }.borrow_mut();
                        match &mut *inner {
                            // Packed ints hold no heap references: nothing to
                            // promote, and the elements are plain i64s.
                            ArrayData::Ints(_) => {}
                            ArrayData::Values(v) => {
                                for e in v.iter_mut() {
                                    walk_value(heap, map, visited, mark.as_deref_mut(), e);
                                }
                            }
                        }
                    }
                    _ => {
                        let copy = new_addr as *const RefCell<ObjectData>;
                        let mut inner = unsafe { &*copy }.borrow_mut();
                        // The proto chain head is a value too: promote it
                        // (class instances keep their prototype reachable).
                        walk_value(heap, map, visited, mark.as_deref_mut(), &mut inner.proto);
                        for e in inner.values.iter_mut() {
                            walk_value(heap, map, visited, mark.as_deref_mut(), e);
                        }
                        // Accessors: the getter/setter functions are heap
                        // values and must be promoted with the box.
                        if let Some(accs) = inner.accessors.as_mut() {
                            for (_, (g, s)) in accs.iter_mut() {
                                walk_value(heap, map, visited, mark.as_deref_mut(), g);
                                walk_value(heap, map, visited, mark.as_deref_mut(), s);
                            }
                        }
                        // Map/Set entries: keys and values are heap values and
                        // must be promoted with the box (the lookup table is
                        // rebuilt against remapped addresses; the order list
                        // is remapped in place).
                        if let Some(cd) = inner.entries.as_mut() {
                            walk_container_entries(
                                cd,
                                heap,
                                map,
                                visited,
                                mark.as_deref_mut(),
                            );
                        }
                    }
                }
                *v = Value(tag | usize_to_payload(new_addr));
            } else if heap.addr_in_old(addr) {
                // Old box: record it for the incremental mark (the budgeted
                // worklist traces its interior later); never trace it inline.
                if let Some(m) = mark {
                    m.mark_box(heap, addr);
                }
            }
            // Else: thread-heap / program-heap constant — untouched.
        }
        TAG_CELL => {
            let addr = payload_to_usize(v.0);
            if visited.insert(addr) {
                let cell = unsafe { &*heap_ptr::<RefCell<Value>>(v.0) };
                walk_value(heap, map, visited, mark.as_deref_mut(), &mut cell.borrow_mut());
            }
        }
        TAG_FN => {
            let addr = payload_to_usize(v.0);
            if visited.insert(addr) {
                let fd = unsafe { &*heap_ptr::<FunctionData>(v.0) };
                for c in &fd.cells {
                    walk_cell(heap, map, visited, mark.as_deref_mut(), c);
                }
                // Class/static properties (the prototype object, static
                // methods) are values the mark must keep alive.
                if let Some(props) = fd.props.borrow().as_ref() {
                    let mut guard = props.borrow_mut();
                    for e in guard.values_mut() {
                        walk_value(heap, map, visited, mark.as_deref_mut(), e);
                    }
                }
            }
        }
        TAG_MISC => match v.as_misc() {
            Some(MiscBox::Channel(st)) => {
                let addr = Arc::as_ptr(st) as usize;
                if visited.insert(addr) {
                    let mut g = st.lock().unwrap_or_else(|g| g.into_inner());
                    for m in g.queue.iter_mut() {
                        // Raw messages are heap values (anonymous channels);
                        // Bytes are plain serialized data with no heap refs.
                        if let ChannelItem::Raw(v) = m {
                            walk_value(heap, map, visited, mark.as_deref_mut(), v);
                        }
                    }
                    for w in g.waiters.iter_mut() {
                        walk_value(heap, map, visited, mark.as_deref_mut(), w);
                    }
                }
            }
            Some(MiscBox::Promise(p)) => {
                let addr = Arc::as_ptr(p) as usize;
                if visited.insert(addr) {
                    let mut ps = p.lock().unwrap_or_else(|g| g.into_inner());
                    match &mut ps.status {
                        PromiseStatus::Fulfilled(val) | PromiseStatus::Rejected(val) => {
                            walk_value(heap, map, visited, mark.as_deref_mut(), val)
                        }
                        PromiseStatus::Pending => {}
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// Drop one arena box's contents by kind — arrays own their element `Vec`,
/// objects own the whole `ObjectData` (shape `Rc`, property values,
/// tombstones) — releasing the `Rc`s they held. Shared by the sweep paths
/// (dead boxes) and the heap's teardown walk (every live box). The caller
/// must hold no outstanding `RefCell` borrows into the box being dropped
/// (the VM is quiescent at unit boundaries and at teardown).
pub fn drop_box_contents(addr: usize, kind: u64) {
    match kind {
        KIND_ARRAY => unsafe {
            drop(std::ptr::read(addr as *const RefCell<ArrayData>));
        },
        KIND_OBJECT => unsafe {
            drop(std::ptr::read(addr as *const RefCell<ObjectData>));
        },
        // KIND_RAW (string bytes / opaque), KIND_STRING (AString box) and
        // KIND_FREE (contents already reclaimed by a sweep) hold nothing
        // droppable.
        _ => {}
    }
}

/// A box-walk callback that drops the contents of every box NOT in `map`
/// (promoted boxes' data moved to the old copy, so their young shells must
/// not be dropped), releasing the `Rc`s they held. The caller must hold no
/// outstanding `RefCell` borrows into the arena being swept (the VM is
/// quiescent at unit boundaries).
fn drop_dead(map: &PromoteMap) -> impl FnMut(usize, u64, usize) + '_ {
    move |addr, kind, _size| {
        if map.contains_key(&addr) {
            return;
        }
        drop_box_contents(addr, kind);
    }
}

/// Drop the contents of every young box that was not relocated, then
/// bulk-reset the young generation (the per-unit minor pass).
pub fn sweep_young(heap: &mut ArenaHeap, map: &PromoteMap) {
    heap.for_each_young_box(drop_dead(map));
    heap.reset_young();
}

/// Second-generation sweep (non-copying): drop the contents of every OLD box
/// not in the mark set (releasing the `Rc`s it held) and coalesce its space
/// onto the old generation's free list for reuse by future promotions. Live
/// boxes are never moved. The caller must hold no outstanding `RefCell`
/// borrows into the old generation (the VM is quiescent at unit boundaries).
pub fn sweep_old_mark_sweep(heap: &mut ArenaHeap, live: &HashSet<usize>) {
    heap.sweep_old(live, |addr, kind| drop_box_contents(addr, kind));
}
