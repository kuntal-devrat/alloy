use crate::ast::Stmt;

/// How a function's captured variable is reached at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpvalueKind {
    /// A local slot in the immediately enclosing function's frame.
    Local { slot: u8 },
    /// An upvalue index in the immediately enclosing function's closure cells.
    Upvalue { index: u8 },
    /// An arrow's hidden lexical capture: the enclosing frame's `this` (or
    /// `arguments`), read via LoadThis/LoadArguments at NewClosure time and
    /// stored in a cell. Arrows never bind their own `this`/`arguments`.
    Lexical,
}

#[derive(Debug, Clone)]
pub struct UpvalueRef {
    pub name: String,
    pub kind: UpvalueKind,
}

#[derive(Debug, Clone)]
pub struct FuncCtx {
    pub locals: Vec<String>,
    pub upvalues: Vec<UpvalueRef>,
    /// Whether this function is `async` (its body may contain `await`).
    pub is_async: bool,
    /// Set when the body references `arguments`: the VM then snapshots the
    /// passed args into the call frame at entry (the frame's local slots
    /// overwrite the arg region as the body runs, so a lazy read would see
    /// locals, not args). Serialized in the NewClosure operand; functions
    /// that never touch `arguments` pay nothing.
    pub uses_arguments: bool,
    /// Whether this function is an arrow (`x => ...`). Arrows bind `this`
    /// and `arguments` lexically: their bodies reference hidden `\0this` /
    /// `\0arguments` upvalues captured at creation, never LoadThis/
    /// LoadArguments of their own frame.
    pub is_arrow: bool,
}

impl FuncCtx {
    pub fn new(is_async: bool, is_arrow: bool) -> Self {
        Self {
            locals: Vec::new(),
            upvalues: Vec::new(),
            is_async,
            uses_arguments: false,
            is_arrow,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopCtx {
    pub break_jumps: Vec<usize>,
    pub continue_jumps: Vec<usize>,
    pub continue_target: usize,
    /// `trys.len()` when this loop was pushed. `break`/`continue` run the
    /// finallys of trys nested *inside* the loop only — trys enclosing the
    /// loop are not exited, so their finallys must not run at the exit site.
    pub trys_depth: usize,
    /// Name of a label directly attached to this loop (`outer: for ...`), if
    /// any; its `continue` jumps are patched together with this loop's.
    pub label: Option<String>,
    /// Switch contexts are `break` targets but not `continue` targets; an
    /// unlabeled `continue` skips them and targets the enclosing loop.
    pub is_switch: bool,
}

/// Where a `break`/`continue` jump is recorded: an index into `loops` (the
/// innermost break/continue target), or into `labels`.
#[derive(Debug, Clone, Copy)]
pub enum ExitTarget {
    Loop(usize),
    Label(usize),
}

/// A labeled statement being compiled: `name: stmt`. `break name` jumps past
/// it; `continue name` is only legal when the label is on a loop.
#[derive(Debug, Clone)]
pub struct LabelCtx {
    pub name: String,
    pub is_loop: bool,
    /// `trys.len()` when the label was pushed — a labeled exit runs the
    /// finallys of trys between the exit site and the labeled statement.
    pub trys_depth: usize,
    pub break_jumps: Vec<usize>,
    pub continue_jumps: Vec<usize>,
    pub continue_target: usize,
}

/// An active `try` while compiling its body. `finally` bodies are re-emitted
/// inline at every `break`/`continue`/`return` exit site (JS semantics).
#[derive(Debug, Clone)]
pub struct TryCtx {
    pub finally: Option<Box<Stmt>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Local(u8),
    Upvalue(u8),
    Global(u16),
}

/// Reserved global holding a module's `export default` value. Collision-proof:
/// `\0` cannot appear in source identifiers, so no user binding shadows it.
pub const DEFAULT_EXPORT: &str = "\0default";

pub fn is_native(name: &str) -> bool {
    matches!(
        name,
        "print" | "http" | "memory" | "fs" | "Promise" | "setTimeout" | "setInterval"
            | "clearTimeout" | "clearInterval" | "queueMicrotask" | "console" | "channel"
            | "spawn" | "Date" | "Math" | "JSON" | "Number" | "Object" | "Array" | "String"
            | "parseInt" | "parseFloat" | "isNaN" | "require" | "reload" | "sweepSegments"
            | "fetchSync" | "crypto" | "URL" | "encodeURIComponent" | "decodeURIComponent"
            | "encodeURI" | "decodeURI" | "btoa" | "atob"
            | "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError"
            | "EvalError" | "URIError" | "NaN" | "Infinity"
    )
}
