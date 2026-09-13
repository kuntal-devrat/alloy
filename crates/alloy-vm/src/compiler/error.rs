#[derive(Debug)]
pub enum CompileError {
    UnexpectedToken(String),
    UnterminatedString,
    InvalidEscape,
    InvalidNumber(String),
    UndefinedVariable(String),
    CannotShadowBuiltin(String),
    BreakOutsideLoop,
    AwaitOutsideAsync,
    UndefinedLabel(String),
    ContinueNonLoop(String),
    /// `counter = 5` where `counter` came from `import { counter }`: ESM
    /// rejects this as a SyntaxError — assigning to a live-import cell would
    /// mutate the module, so it is a loud compile error here too.
    AssignToImport(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedToken(s) => write!(f, "unexpected token: {}", s),
            Self::UnterminatedString => write!(f, "unterminated string literal"),
            Self::InvalidEscape => write!(f, "invalid escape sequence"),
            Self::InvalidNumber(s) => write!(f, "invalid number: {}", s),
            Self::UndefinedVariable(s) => write!(f, "undefined variable: {}", s),
            Self::CannotShadowBuiltin(s) => write!(f, "cannot shadow builtin '{}'", s),
            Self::BreakOutsideLoop => write!(f, "break/continue outside of a loop"),
            Self::AwaitOutsideAsync => write!(f, "'await' is only allowed inside an async function"),
            Self::UndefinedLabel(s) => write!(f, "undefined label: {}", s),
            Self::ContinueNonLoop(s) => write!(f, "'continue' to non-loop label: {}", s),
            Self::AssignToImport(s) => write!(f, "cannot assign to imported binding '{}'", s),
        }
    }
}

impl std::error::Error for CompileError {}
