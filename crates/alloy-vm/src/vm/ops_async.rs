use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use alloy_core::value::{PromiseState, Value};
use crate::vm::stack::CallFrame;

#[derive(Clone)]
pub struct Microtask {
    pub id: u64,
    /// The fulfillment value or rejection reason.
    pub value: Value,
    /// True when the settlement was a rejection.
    pub rejected: bool,
}

/// A saved execution: either a suspended async invocation (resumed with a
/// settled value) or a `.then` callback to invoke on settlement.
pub enum Continuation {
    Suspended {
        /// Operand stack of the async invocation, without the awaited value.
        stack: Vec<Value>,
        /// The async frame and everything it called, top last.
        frames: Vec<CallFrame>,
        cells: Vec<Vec<Rc<RefCell<Value>>>>,
        /// Active exception handlers owned by the saved frames.
        handlers: Vec<Handler>,
        /// Resume pc (after the `Await` opcode) in `program_id`.
        pc: usize,
        program_id: u32,
    },
    Callback {
        /// Optional `.then(onFulfilled)` handler (None = pass through).
        callback: Option<Value>,
        /// Optional `.then(_, onRejected)` handler (None = pass through).
        on_rejected: Option<Value>,
        /// Chained promise resolved with the callback's result.
        promise: Arc<Mutex<PromiseState>>,
    },
}

/// An active `try` block: where to unwind the stack to and where the handler
/// code lives.
#[derive(Clone)]
pub struct Handler {
    /// Stack depth at TryStart, in the owning frame's absolute stack.
    pub stack_depth: usize,
    /// Handler code entry (the thrown value is pushed at this pc).
    pub handler_pc: usize,
    /// Index of the frame that owns this handler (call_stack position).
    pub frame_depth: usize,
    /// Program the handler bytecode belongs to.
    pub program: u32,
}

/// Result of routing a thrown value through the unwinder.
pub enum ThrowResult {
    /// Continue the dispatch loop at this pc.
    Jump(usize),
    /// The throw ended the current dispatch (resumed continuation boundary).
    EndDispatch,
    /// No handler anywhere: the VM recorded `uncaught_exception`.
    Abort,
}

pub struct Timer {
    /// Deadline in ms since the VM's `epoch`.
    pub when: f64,
    /// Insertion order, for FIFO firing of equal deadlines.
    pub seq: u64,
    /// Monotonic handle returned to JS (`setTimeout`/`setInterval` id).
    pub id: u64,
    /// `Some(period)` for `setInterval`; `None` for one-shot `setTimeout`.
    pub period: Option<f64>,
    pub callback: Value,
}
