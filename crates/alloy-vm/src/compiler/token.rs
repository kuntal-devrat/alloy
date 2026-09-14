/// Tokens plus the source line each token starts on (1-based). The parser uses
/// the lines to tell a same-line adjacency error (`print(1 2)`) from a
/// statement boundary at a newline (`const f = x => x` then `print(f())` —
/// the engine treats a newline as an implicit statement separator).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenStream {
    pub tokens: Vec<Token>,
    pub lines: Vec<u32>,
    pub cols: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TemplatePart {
    /// Literal text between `${...}` interpolations.
    Lit(String),
    /// Tokens of an interpolated expression, parsed by the parser.
    Expr(TokenStream),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Number(f64),
    Int(i64),
    StringLit(String),
    TemplateLit(Vec<TemplatePart>),
    True,
    False,
    Null,
    Undefined,
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    EqEq,
    EqEqEq,
    Neq,
    NeqEq,
    Lt,
    Gt,
    Lte,
    Gte,
    Not,
    And,
    Or,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    Shl,
    Shr,
    UShr,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    StarStar,
    StarStarAssign,
    PlusPlus,
    MinusMinus,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Colon,
    Dot,
    DotDotDot,
    Arrow,
    QuestionDot,
    QuestionQuestion,
    AndAssign,
    OrAssign,
    NullishAssign,
    Let,
    Const,
    Var,
    Function,
    Return,
    If,
    Else,
    While,
    Do,
    For,
    In,
    Of,
    Import,
    Export,
    From,
    As,
    Async,
    Await,
    New,
    Typeof,
    Void,
    Delete,
    Question,
    Break,
    Continue,
    Try,
    Catch,
    Finally,
    Throw,
    Switch,
    Case,
    Default,
    Class,
    Extends,
    Super,
    This,
    Static,
    InstanceOf,
    Yield,
    PrivateIdent(String),
    /// `/pattern/flags` — the lexer already validated pattern syntax and
    /// flags against Node's rules (loud compile error otherwise).
    Regex {
        pattern: String,
        flags: String,
    },
    Eof,
}

/// Map a keyword token back to its source text. Keywords are legal property
/// names in JS (the IdentifierName rule): `m.default`, `o.delete`, `{ if: 1 }`
/// all parse even though the words are reserved. Returns None for non-keyword
/// tokens (operators, literals) that cannot name a property.
pub fn keyword_text(t: &Token) -> Option<String> {
    Some(match t {
        Token::Ident(s) => return Some(s.clone()),
        Token::PrivateIdent(s) => return Some(s.clone()),
        Token::True => "true".into(),
        Token::False => "false".into(),
        Token::Null => "null".into(),
        Token::Undefined => "undefined".into(),
        Token::Let => "let".into(),
        Token::Const => "const".into(),
        Token::Var => "var".into(),
        Token::Function => "function".into(),
        Token::Return => "return".into(),
        Token::If => "if".into(),
        Token::Else => "else".into(),
        Token::While => "while".into(),
        Token::Do => "do".into(),
        Token::For => "for".into(),
        Token::In => "in".into(),
        Token::Of => "of".into(),
        Token::Import => "import".into(),
        Token::Export => "export".into(),
        Token::From => "from".into(),
        Token::As => "as".into(),
        Token::Async => "async".into(),
        Token::Await => "await".into(),
        Token::New => "new".into(),
        Token::Typeof => "typeof".into(),
        Token::Void => "void".into(),
        Token::Delete => "delete".into(),
        Token::Break => "break".into(),
        Token::Continue => "continue".into(),
        Token::Try => "try".into(),
        Token::Catch => "catch".into(),
        Token::Finally => "finally".into(),
        Token::Throw => "throw".into(),
        Token::Switch => "switch".into(),
        Token::Case => "case".into(),
        Token::Default => "default".into(),
        Token::Class => "class".into(),
        Token::Extends => "extends".into(),
        Token::Super => "super".into(),
        Token::This => "this".into(),
        Token::Static => "static".into(),
        Token::InstanceOf => "instanceof".into(),
        Token::Yield => "yield".into(),
        _ => return None,
    })
}

/// A token that can START an expression. When one of these directly follows a
/// complete expression (no operator, no postfix), the source is invalid JS.
pub fn starts_expression(t: &Token) -> bool {
    matches!(
        t,
        Token::Ident(_)
            | Token::Number(_)
            | Token::Int(_)
            | Token::StringLit(_)
            | Token::True
            | Token::False
            | Token::Null
            | Token::Undefined
            | Token::LBracket
            | Token::Function
            | Token::TemplateLit(_)
            | Token::Async
            | Token::New
            | Token::Typeof
            | Token::Void
            | Token::Delete
            | Token::Not
            | Token::BitNot
            | Token::Await
            | Token::Let
            | Token::Const
            | Token::Var
            | Token::If
            | Token::While
            | Token::For
            | Token::Switch
            | Token::Try
            | Token::Return
            | Token::Break
            | Token::Continue
            | Token::Throw
            | Token::Case
            | Token::Default
            | Token::Else
            | Token::Catch
            | Token::Finally
            | Token::Import
            | Token::Export
            | Token::Of
            | Token::Class
            | Token::This
            | Token::Super
            | Token::Yield
            | Token::PrivateIdent(_)
    )
}
