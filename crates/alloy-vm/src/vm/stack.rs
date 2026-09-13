use alloy_core::value::Value;

pub const STACK_SIZE: usize = 16384;
pub const FRAME_BUDGET: usize = 512;
pub const MAX_CALL_DEPTH: usize = 512;

pub const KIND_UNKNOWN: u8 = 0;
pub const KIND_INT: u8 = 1;
pub const KIND_NUMBER: u8 = 2;
pub const KIND_OTHER: u8 = 3;

#[inline(always)]
pub fn kind_of_value(v: &Value) -> u8 {
    if v.is_int() {
        KIND_INT
    } else if v.is_number() {
        KIND_NUMBER
    } else {
        KIND_OTHER
    }
}

pub struct OperandStack {
    pub slots: Box<[Value]>,
    /// Parallel per-slot type feedback (KIND_* constants): exact for
    /// every live slot — updated on every write that can change a slot's
    /// contents. Pops/truncates leave dead slots stale, which is safe because
    /// the next push that reuses the index invalidates the entry.
    pub kinds: Box<[u8]>,
    pub sp: usize,
    /// ALLOY_NO_SMI_FB=1 disables feedback collection: kinds stay UNKNOWN,
    /// so every fast lane dead-ends (the A/B switch for measuring the win).
    pub fb: bool,
}

impl OperandStack {
    pub fn new() -> Self {
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
    pub fn push(&mut self, val: Value) {
        debug_assert!(self.sp < STACK_SIZE, "operand stack overflow");
        unsafe {
            *self.slots.get_unchecked_mut(self.sp) = val;
        }
        if self.fb {
            unsafe {
                *self.kinds.get_unchecked_mut(self.sp) = KIND_OTHER;
            }
        }
        self.sp += 1;
    }

    /// Feedback kind of a slot (valid index required, as with `at`).
    #[inline(always)]
    pub fn kind_of(&self, i: usize) -> u8 {
        unsafe { *self.kinds.get_unchecked(i) }
    }

    /// Record the kind of a slot after a store path wrote to it.
    #[inline(always)]
    pub fn mark_kind(&mut self, i: usize, k: u8) {
        if self.fb {
            unsafe {
                *self.kinds.get_unchecked_mut(i) = k;
            }
        }
    }

    #[inline(always)]
    pub fn pop(&mut self) -> Value {
        if self.sp > 0 {
            self.sp -= 1;
            unsafe { std::mem::replace(self.slots.get_unchecked_mut(self.sp), Value::undefined()) }
        } else {
            Value::undefined()
        }
    }

    #[inline(always)]
    pub fn peek(&self) -> Value {
        if self.sp > 0 {
            self.slots[self.sp - 1].clone()
        } else {
            Value::undefined()
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.sp
    }

    #[inline]
    pub fn at(&self, i: usize) -> &Value {
        debug_assert!(i < self.sp);
        unsafe { self.slots.get_unchecked(i) }
    }

    #[inline]
    pub fn at_mut(&mut self, i: usize) -> &mut Value {
        debug_assert!(i < self.sp);
        unsafe { self.slots.get_unchecked_mut(i) }
    }

    /// Drop everything above `n` (function return, exception unwind). Slots
    /// are cleared so references are released and every slot stays valid.
    #[inline]
    pub fn truncate(&mut self, n: usize) {
        let n = n.min(self.sp);
        for s in &mut self.slots[n..self.sp] {
            *s = Value::undefined();
        }
        self.sp = n;
    }

    #[inline]
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Saved portion `[base..sp]`, used to suspend async invocations.
    pub fn save_from(&self, base: usize) -> Vec<Value> {
        self.slots[base..self.sp].to_vec()
    }

    /// Replace the whole stack with a saved continuation (never larger than
    /// `STACK_SIZE` — it was saved from this stack).
    pub fn restore(&mut self, values: Vec<Value>) {
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

#[derive(Clone)]
pub struct CallFrame {
    pub return_addr: usize,
    /// Program to resume in on return (index into `Vm::programs`).
    pub return_program: u32,
    pub base_slot: usize,
    /// Number of arguments actually passed.
    pub argc: usize,
    /// Snapshot of the passed args for `arguments`.
    pub arg_values: Option<Vec<Value>>,
    /// The function value of the current frame (for LoadSelf recursion).
    pub fn_value: Value,
    /// cells_stack depth before this frame's cells were pushed.
    pub cells_len: usize,
    /// Local slot holding this invocation's promise, for async functions.
    pub promise_slot: Option<u8>,
    /// Generator ID for generator function invocations.
    pub generator_id: Option<u64>,
    /// True when this frame was restored from a continuation.
    pub resumed: bool,
    /// Whether this call's result is pushed back to the caller.
    pub keep_result: bool,
    /// Length of the global handler stack when this frame was pushed.
    pub handlers_len: usize,
    /// One past the highest local slot written in this frame.
    pub locals_end: usize,
    /// Operand-stack slot holding the receiver (`this`) of a method/new call.
    pub this_slot: Option<usize>,
    /// True for a `new` invocation.
    pub is_ctor: bool,
}
