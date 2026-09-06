use crate::bytecode::Program;
use crate::opcode::Opcode;
use alloy_core::heap::{ArenaHeap, HeapGuard};
use alloy_core::value::Value;

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

/// Tokens plus the source line each token starts on (1-based). The parser uses
/// the lines to tell a same-line adjacency error (`print(1 2)`) from a
/// statement boundary at a newline (`const f = x => x` then `print(f())` —
/// the engine treats a newline as an implicit statement separator).
#[derive(Debug, Clone, PartialEq)]
struct TokenStream {
    tokens: Vec<Token>,
    lines: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
enum TemplatePart {
    /// Literal text between `${...}` interpolations.
    Lit(String),
    /// Tokens of an interpolated expression, parsed by the parser.
    Expr(TokenStream),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Int(i64),
    StringLit(String),
    TemplateLit(Vec<TemplatePart>),
    True, False, Null, Undefined,
    Ident(String),
    Plus, Minus, Star, Slash, Percent,
    Assign, EqEq, EqEqEq, Neq, NeqEq,
    Lt, Gt, Lte, Gte,
    Not, And, Or, PlusAssign, MinusAssign, StarAssign, SlashAssign, PercentAssign,
    BitAnd, BitOr, BitXor, BitNot,
    BitAndAssign, BitOrAssign, BitXorAssign,
    Shl, Shr, UShr, ShlAssign, ShrAssign, UShrAssign,
    StarStar, StarStarAssign,
    PlusPlus, MinusMinus,
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Semicolon, Comma, Colon, Dot, DotDotDot, Arrow, QuestionDot, QuestionQuestion,
    AndAssign, OrAssign, NullishAssign,
    Let, Const, Var, Function, Return, If, Else,
    While, Do, For, In, Of, Import, Export, From, As,
    Async, Await, New, Typeof, Void, Delete, Question,
    Break, Continue, Try, Catch, Finally, Throw,
    Switch, Case, Default,
    Class, Extends, Super, This, Static, InstanceOf,
    /// `/pattern/flags` — the lexer already validated pattern syntax and
    /// flags against Node's rules (loud compile error otherwise).
    Regex { pattern: String, flags: String },
    Eof,
}

struct Lexer {
    src: Vec<char>,
    pos: usize,
    /// `line_of[i]` = the 1-based source line of the char at index `i` (one
    /// extra entry so `pos == src.len()` at Eof is valid).
    line_of: Vec<u32>,
}

impl Lexer {
    fn new(s: &str) -> Self {
        let src: Vec<char> = s.chars().collect();
        let mut line_of = vec![1u32; src.len() + 1];
        let mut line = 1u32;
        for (i, &c) in src.iter().enumerate() {
            if c == '\n' {
                line += 1;
            }
            line_of[i + 1] = line;
        }
        Self { src, pos: 0, line_of }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn adv(&mut self) -> Option<char> {
        let c = self.src.get(self.pos).copied();
        if c.is_some() { self.pos += 1; }
        c
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.adv();
            } else if ch == '/' && self.pos + 1 < self.src.len() {
                if self.src[self.pos + 1] == '/' {
                    while self.peek() != Some('\n') && self.peek().is_some() {
                        self.adv();
                    }
                } else if self.src[self.pos + 1] == '*' {
                    self.adv();
                    self.adv();
                    loop {
                        if let Some(c) = self.adv() {
                            if c == '*' && self.peek() == Some('/') {
                                self.adv();
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn read_hex_digit(&mut self) -> Result<u32, CompileError> {
        match self.adv().and_then(|c| c.to_digit(16)) {
            Some(d) => Ok(d),
            None => Err(CompileError::InvalidEscape),
        }
    }

    fn read_hex4(&mut self) -> Result<u32, CompileError> {
        let mut n = 0u32;
        for _ in 0..4 {
            n = n * 16 + self.read_hex_digit()?;
        }
        Ok(n)
    }

    /// Full JS string escapes: `\n \t \r \b \f \v \0 \\ \" \' \xNN
    /// \uNNNN \u{...}` (surrogate pairs combined), line continuations
    /// (`\<newline>` → nothing), and the JS rule that an unknown escape
    /// drops the backslash (`\q` → `q`).
    fn read_str(&mut self, q: char) -> Result<String, CompileError> {
        let mut s = String::new();
        loop {
            match self.adv() {
                None => return Err(CompileError::UnterminatedString),
                Some(c) if c == q => break,
                Some('\\') => match self.adv() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{C}'),
                    Some('v') => s.push('\u{B}'),
                    Some('0') => s.push('\0'),
                    Some('\\') => s.push('\\'),
                    Some('"') => s.push('"'),
                    Some('\'') => s.push('\''),
                    Some('x') => {
                        let hi = self.read_hex_digit()?;
                        let lo = self.read_hex_digit()?;
                        s.push(char::from_u32(hi * 16 + lo).unwrap_or('\u{FFFD}'));
                    }
                    Some('u') => {
                        if self.peek() == Some('{') {
                            self.adv();
                            let mut n: u32 = 0;
                            let mut any = false;
                            loop {
                                match self.peek() {
                                    Some('}') => {
                                        self.adv();
                                        break;
                                    }
                                    Some(c) => match c.to_digit(16) {
                                        Some(d) => {
                                            n = n * 16 + d;
                                            any = true;
                                            self.adv();
                                        }
                                        None => return Err(CompileError::InvalidEscape),
                                    },
                                    None => return Err(CompileError::InvalidEscape),
                                }
                            }
                            if !any {
                                return Err(CompileError::InvalidEscape);
                            }
                            s.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                        } else {
                            let hi = self.read_hex4()?;
                            if (0xD800..=0xDBFF).contains(&hi) && self.peek() == Some('\\') {
                                // Surrogate pair `\uD800\uDC00` → one code point.
                                let save = self.pos;
                                self.adv();
                                if self.adv() == Some('u') {
                                    let lo = self.read_hex4()?;
                                    if (0xDC00..=0xDFFF).contains(&lo) {
                                        let cp =
                                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                    } else {
                                        self.pos = save;
                                        s.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                                    }
                                } else {
                                    self.pos = save;
                                    s.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                                }
                            } else {
                                s.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        }
                    }
                    // Line continuation: backslash-newline produces nothing.
                    Some('\n') => {}
                    Some(c) => s.push(c),
                    None => return Err(CompileError::UnterminatedString),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    /// Scan a template literal body starting after the opening backtick.
    /// Produces literal parts and recursively-tokenized `${...}` expressions.
    fn read_template(&mut self) -> Result<Token, CompileError> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            match self.adv() {
                None => return Err(CompileError::UnterminatedString),
                Some('`') => break,
                Some('\\') => match self.adv() {
                    Some('n') => lit.push('\n'),
                    Some('t') => lit.push('\t'),
                    Some('`') => lit.push('`'),
                    Some('$') => lit.push('$'),
                    Some('\\') => lit.push('\\'),
                    Some(c) => lit.push(c),
                    None => return Err(CompileError::UnterminatedString),
                },
                Some('$') if self.peek() == Some('{') => {
                    self.adv();
                    if !lit.is_empty() {
                        parts.push(TemplatePart::Lit(std::mem::take(&mut lit)));
                    }
                    let raw = self.read_template_expr_raw()?;
                    let toks = Lexer::new(&raw).tokenize()?;
                    parts.push(TemplatePart::Expr(toks));
                }
                Some(c) => lit.push(c),
            }
        }
        if !lit.is_empty() {
            parts.push(TemplatePart::Lit(lit));
        }
        Ok(Token::TemplateLit(parts))
    }

    /// Collect the raw text of a `${ ... }` expression, tracking brace depth
    /// and skipping strings and nested templates so braces inside them don't
    /// terminate the segment early.
    fn read_template_expr_raw(&mut self) -> Result<String, CompileError> {
        let mut s = String::new();
        let mut depth = 1usize;
        loop {
            match self.adv() {
                None => return Err(CompileError::UnterminatedString),
                Some('{') => {
                    depth += 1;
                    s.push('{');
                }
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(s);
                    }
                    s.push('}');
                }
                Some('"') | Some('\'') => {
                    let q = self.src[self.pos - 1];
                    s.push(q);
                    loop {
                        match self.adv() {
                            None => return Err(CompileError::UnterminatedString),
                            Some('\\') => {
                                s.push('\\');
                                if let Some(e) = self.adv() {
                                    s.push(e);
                                }
                            }
                            Some(c) if c == q => {
                                s.push(c);
                                break;
                            }
                            Some(c) => s.push(c),
                        }
                    }
                }
                Some('`') => {
                    s.push('`');
                    self.scan_template_raw(&mut s)?;
                }
                Some(c) => s.push(c),
            }
        }
    }

    /// Scan one nested template literal into `out`, starting just after its
    /// opening backtick.
    fn scan_template_raw(&mut self, out: &mut String) -> Result<(), CompileError> {
        loop {
            match self.adv() {
                None => return Err(CompileError::UnterminatedString),
                Some('`') => {
                    out.push('`');
                    return Ok(());
                }
                Some('\\') => {
                    out.push('\\');
                    if let Some(e) = self.adv() {
                        out.push(e);
                    }
                }
                Some('$') if self.peek() == Some('{') => {
                    self.adv();
                    out.push_str("${");
                    let raw = self.read_template_expr_raw()?;
                    out.push_str(&raw);
                    out.push('}');
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn read_num(&mut self, first: char) -> Result<Token, CompileError> {
        // Radix-prefixed literals: `0x`/`0X` hex, `0b`/`0B` binary, `0o`/`0O`
        // octal. Anything after the prefix that isn't a digit of that radix
        // (including a missing digit run) is a loud error, like JS — these
        // used to silently misparse (`0xFF` became `0` + identifier `xFF`).
        if first == '0' {
            match self.peek() {
                Some('x') | Some('X') => return self.read_radix_num(16),
                Some('b') | Some('B') => return self.read_radix_num(2),
                Some('o') | Some('O') => return self.read_radix_num(8),
                _ => {}
            }
        }
        let mut s = String::new();
        s.push(first);
        // A leading `.` (`read_num('.')` for `.5`) is itself the fraction
        // point — subsequent dots start a member access.
        let mut fl = first == '.';
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                s.push(ch);
                self.adv();
            } else if ch == '.' && !fl {
                fl = true;
                s.push(ch);
                self.adv();
            } else {
                break;
            }
        }
        // Optional exponent part: `e`/`E` followed by [sign] digits. Only
        // consumed when a digit follows, so `1ex` stays `1` + ident `ex`.
        if matches!(self.peek(), Some('e') | Some('E')) {
            let at = |lex: &Lexer, off: usize| lex.src.get(lex.pos + off).copied();
            let has_exp = match (at(self, 1), at(self, 2)) {
                (Some(d), _) if d.is_ascii_digit() => true,
                (Some('+') | Some('-'), Some(d)) if d.is_ascii_digit() => true,
                _ => false,
            };
            if has_exp {
                s.push(self.adv().unwrap()); // e / E
                if matches!(self.peek(), Some('+') | Some('-')) {
                    s.push(self.adv().unwrap());
                }
                while let Some(ch) = self.peek() {
                    if ch.is_ascii_digit() {
                        s.push(ch);
                        self.adv();
                    } else {
                        break;
                    }
                }
                fl = true;
            }
        }
        // A BigInt suffix (`10n`, `1.5n`) immediately after any numeric
        // literal is not supported — error loudly instead of misparsing `n`
        // as an identifier (`print(10n)` used to print `10 undefined`).
        if self.peek() == Some('n') {
            return Err(CompileError::InvalidNumber(
                "BigInt literals are not supported".to_string(),
            ));
        }
        // Legacy octal: an integer literal with a leading zero whose digits
        // are all 0-7 is octal in sloppy JS (`017` = 15); a non-octal digit
        // (`08`, `09`) falls back to decimal. A fraction or exponent on a
        // legacy-octal literal is a SyntaxError (`01.5`, `017e2`).
        let int_part = s.split(['.', 'e', 'E']).next().unwrap_or(&s);
        if int_part.len() > 1
            && int_part.starts_with('0')
            && int_part.bytes().all(|b| (b'0'..=b'7').contains(&b))
        {
            if fl {
                return Err(CompileError::InvalidNumber(
                    "legacy octal literals cannot have a fraction or exponent".to_string(),
                ));
            }
            return match u128::from_str_radix(int_part, 8) {
                Ok(v) if v <= i64::MAX as u128 => Ok(Token::Int(v as i64)),
                Ok(v) => Ok(Token::Number(v as f64)),
                Err(_) => Ok(Token::Number(f64::NAN)),
            };
        }
        if fl {
            Ok(Token::Number(s.parse().unwrap_or(0.0)))
        } else {
            match s.parse::<i64>() {
                Ok(v) => Ok(Token::Int(v)),
                // Literal too big for i64 (`99999999999999999999`): JS reads
                // it as the f64 it rounds to.
                Err(_) => Ok(Token::Number(s.parse().unwrap_or(0.0))),
            }
        }
    }

    /// A radix-prefixed literal (`0x…`, `0b…`, `0o…`); the leading `0` has
    /// already been consumed by `read_num`.
    fn read_radix_num(&mut self, radix: u32) -> Result<Token, CompileError> {
        let digits: &str = match radix {
            16 => "0123456789abcdef",
            2 => "01",
            _ => "01234567",
        };
        self.adv(); // consume x / b / o
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            // A `n` suffix makes this a BigInt literal (`0x10n`) — handled
            // below with the dedicated message.
            if ch == 'n' {
                break;
            }
            if ch.is_ascii_alphanumeric() || ch == '_' {
                if digits.contains(ch.to_ascii_lowercase()) {
                    s.push(ch);
                    self.adv();
                } else {
                    // `0b2`, `0o8`, `0xG` are SyntaxErrors in JS.
                    return Err(CompileError::InvalidNumber(format!(
                        "invalid digit '{}' in radix-{} literal",
                        ch, radix
                    )));
                }
            } else {
                break;
            }
        }
        if s.is_empty() {
            return Err(CompileError::InvalidNumber(
                "missing digits in radix literal".to_string(),
            ));
        }
        // `0x10n`, `0b101n` are BigInt literals — unsupported, loud error.
        if self.peek() == Some('n') {
            return Err(CompileError::InvalidNumber(
                "BigInt literals are not supported".to_string(),
            ));
        }
        // Radix literals cannot carry a fraction or exponent. A `.` after a
        // radix literal starts a MEMBER ACCESS, not a fraction: `0x10.toString`
        // is `(0x10).toString` (V8 accepts it; `0x1.5` still errors below, in
        // the parser, as a member access with a numeric property). `e`/`E` are
        // hex digits (consumed above for radix 16) or invalid digits (radix
        // 2/8, caught above) — so nothing to reject here.
        match u128::from_str_radix(&s, radix) {
            Ok(v) if v <= i64::MAX as u128 => Ok(Token::Int(v as i64)),
            Ok(v) => Ok(Token::Number(v as f64)),
            // Absurdly long literals beyond u128: fold to f64.
            Err(_) => {
                let mut v = 0.0f64;
                for c in s.chars() {
                    v = v * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64;
                }
                Ok(Token::Number(v))
            }
        }
    }

    fn read_ident(&mut self, first: char) -> Token {
        let mut s = String::new();
        s.push(first);
        while let Some(ch) = self.peek() {
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                s.push(ch);
                self.adv();
            } else {
                break;
            }
        }
        match s.as_str() {
            "true" => Token::True,
            "false" => Token::False,
            "null" => Token::Null,
            "undefined" => Token::Undefined,
            "let" => Token::Let,
            "const" => Token::Const,
            "var" => Token::Var,
            "function" => Token::Function,
            "return" => Token::Return,
            "if" => Token::If,
            "else" => Token::Else,
            "while" => Token::While,
            "do" => Token::Do,
            "for" => Token::For,
            "in" => Token::In,
            "of" => Token::Of,
            "import" => Token::Import,
            "export" => Token::Export,
            "from" => Token::From,
            "as" => Token::As,
            "async" => Token::Async,
            "await" => Token::Await,
            "new" => Token::New,
            "typeof" => Token::Typeof,
            "void" => Token::Void,
            "delete" => Token::Delete,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "try" => Token::Try,
            "catch" => Token::Catch,
            "finally" => Token::Finally,
            "throw" => Token::Throw,
            "switch" => Token::Switch,
            "case" => Token::Case,
            "default" => Token::Default,
            "class" => Token::Class,
            "extends" => Token::Extends,
            "super" => Token::Super,
            "this" => Token::This,
            "static" => Token::Static,
            "instanceof" => Token::InstanceOf,
            _ => Token::Ident(s),
        }
    }

    /// Push a token, recording the source line its first char starts on.
    fn push_tok(
        &self,
        t: &mut Vec<Token>,
        lines: &mut Vec<u32>,
        start: usize,
        tok: Token,
    ) {
        lines.push(self.line_of[start]);
        t.push(tok);
    }

    /// Can a `/` at this point start a regex literal? JS lexes `/` as a regex
    /// when the previous token cannot end an expression (operators, open
    /// brackets, keywords like `return`/`typeof`/`in`), and as division after
    /// expression-ending tokens (identifiers, literals, `)`, `]`, `}`,
    /// postfix `++`/`--`). The one genuinely ambiguous case (`x = {} / 2`)
    /// is resolved the way real lexers do: `}` never allows a regex.
    fn regex_allowed(&self, prev: Option<&Token>) -> bool {
        let Some(p) = prev else { return true };
        !matches!(
            p,
            Token::Ident(_)
                | Token::Number(_)
                | Token::Int(_)
                | Token::StringLit(_)
                | Token::TemplateLit(_)
                | Token::True
                | Token::False
                | Token::Null
                | Token::Undefined
                | Token::This
                | Token::Super
                | Token::RParen
                | Token::RBracket
                | Token::RBrace
                | Token::PlusPlus
                | Token::MinusMinus
                | Token::Regex { .. }
        )
    }

    /// Read a regex literal: `/pattern/flags`. The pattern is read raw with
    /// `\` escaping (inside and outside character classes, where `/` is
    /// literal); an unterminated pattern, a raw newline, or an invalid flag
    /// string is a compile error, and the pattern is validated by the regex
    /// engine so bad syntax fails at compile time like Node's parse-time
    /// SyntaxError.
    fn read_regex(&mut self) -> Result<(String, String), CompileError> {
        self.adv(); // opening /
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.peek() else {
                return Err(CompileError::UnexpectedToken(
                    "unterminated regular expression literal".to_string(),
                ));
            };
            self.adv();
            match c {
                '\\' => {
                    let Some(e) = self.peek() else {
                        return Err(CompileError::UnexpectedToken(
                            "unterminated regular expression literal".to_string(),
                        ));
                    };
                    pattern.push('\\');
                    pattern.push(e);
                    self.adv();
                }
                '/' if !in_class => break,
                '[' => {
                    in_class = true;
                    pattern.push(c);
                }
                ']' => {
                    in_class = false;
                    pattern.push(c);
                }
                '\n' | '\r' => {
                    return Err(CompileError::UnexpectedToken(
                        "newline not allowed in regular expression literal".to_string(),
                    ));
                }
                _ => pattern.push(c),
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                flags.push(c);
                self.adv();
            } else {
                break;
            }
        }
        let f = alloy_core::regex::RegexFlags::parse(&flags).map_err(|e| {
            CompileError::UnexpectedToken(format!("invalid regular expression: {e}"))
        })?;
        alloy_core::regex::compile(&pattern, f).map_err(|e| {
            CompileError::UnexpectedToken(format!("invalid regular expression: {e}"))
        })?;
        Ok((pattern, flags))
    }

    fn tokenize(&mut self) -> Result<TokenStream, CompileError> {
        let mut t = Vec::new();
        let mut lines = Vec::new();
        loop {
            self.skip_ws();
            let start = self.pos;
            let ch = match self.peek() {
                None => { self.push_tok(&mut t, &mut lines, start, Token::Eof); break; }
                Some(c) => c,
            };
            match ch {
                '(' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::LParen); }
                ')' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::RParen); }
                '{' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::LBrace); }
                '}' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::RBrace); }
                '[' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::LBracket); }
                ']' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::RBracket); }
                ';' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Semicolon); }
                ',' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Comma); }
                ':' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Colon); }
                '?' => {
                    self.adv();
                    // `?.` is optional chaining UNLESS the next char is a
                    // digit (`a ?.5 : 0` is the ternary `a ? 0.5 : 0`, per
                    // the JS spec's lookahead).
                    if self.peek() == Some('.') && !self.src.get(self.pos + 1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        self.adv();
                        self.push_tok(&mut t, &mut lines, start, Token::QuestionDot);
                    } else if self.peek() == Some('?') {
                        self.adv();
                        // `??=` (nullish logical assignment).
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(&mut t, &mut lines, start, Token::NullishAssign);
                        } else {
                            self.push_tok(&mut t, &mut lines, start, Token::QuestionQuestion);
                        }
                    } else {
                        self.push_tok(&mut t, &mut lines, start, Token::Question);
                    }
                }
                '.' => {
                    self.adv();
                    if self.peek() == Some('.') && self.src.get(self.pos + 1) == Some(&'.') {
                        self.adv();
                        self.adv();
                        self.push_tok(&mut t, &mut lines, start, Token::DotDotDot);
                    } else if self.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        // `.5` is a valid number literal in JS (`print(.5)` →
                        // 0.5); the leading dot is the fraction point.
                        let num = self.read_num('.')?;
                        self.push_tok(&mut t, &mut lines, start, num);
                    } else {
                        self.push_tok(&mut t, &mut lines, start, Token::Dot);
                    }
                }
                '+' => {
                    self.adv();
                    if self.peek() == Some('+') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::PlusPlus); }
                    else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::PlusAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Plus); }
                }
                '-' => {
                    self.adv();
                    if self.peek() == Some('-') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::MinusMinus); }
                    else if self.peek() == Some('>') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Arrow); }
                    else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::MinusAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Minus); }
                }
                '*' => {
                    self.adv();
                    if self.peek() == Some('*') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::StarStarAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::StarStar); }
                    } else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::StarAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Star); }
                }
                '/' => {
                    // A `/` after a token that cannot end an expression starts
                    // a regex literal; after an expression-ending token it is
                    // division (`a / b`, `a++ / b`). The previous token is
                    // `t.last()` — the regex branch runs before pushing.
                    if self.regex_allowed(t.last()) {
                        let (pattern, flags) = self.read_regex()?;
                        self.push_tok(&mut t, &mut lines, start, Token::Regex { pattern, flags });
                    } else {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::SlashAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::Slash); }
                    }
                }
                '%' => {
                    self.adv();
                    if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::PercentAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Percent); }
                }
                '=' => {
                    self.adv();
                    if self.peek() == Some('>') {
                        // `=>` arrow function
                        self.adv();
                        self.push_tok(&mut t, &mut lines, start, Token::Arrow);
                    } else if self.peek() == Some('=') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::EqEqEq); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::EqEq); }
                    } else { self.push_tok(&mut t, &mut lines, start, Token::Assign); }
                }
                '!' => {
                    self.adv();
                    if self.peek() == Some('=') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::NeqEq); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::Neq); }
                    } else { self.push_tok(&mut t, &mut lines, start, Token::Not); }
                }
                '<' => {
                    self.adv();
                    if self.peek() == Some('<') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::ShlAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::Shl); }
                    } else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Lte); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Lt); }
                }
                '>' => {
                    self.adv();
                    if self.peek() == Some('>') {
                        self.adv();
                        if self.peek() == Some('>') {
                            self.adv();
                            if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::UShrAssign); }
                            else { self.push_tok(&mut t, &mut lines, start, Token::UShr); }
                        } else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::ShrAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::Shr); }
                    } else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::Gte); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::Gt); }
                }
                '&' => {
                    self.adv();
                    if self.peek() == Some('&') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::AndAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::And); }
                    }
                    else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::BitAndAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::BitAnd); }
                }
                '|' => {
                    self.adv();
                    if self.peek() == Some('|') {
                        self.adv();
                        if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::OrAssign); }
                        else { self.push_tok(&mut t, &mut lines, start, Token::Or); }
                    }
                    else if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::BitOrAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::BitOr); }
                }
                '^' => {
                    self.adv();
                    if self.peek() == Some('=') { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::BitXorAssign); }
                    else { self.push_tok(&mut t, &mut lines, start, Token::BitXor); }
                }
                '~' => { self.adv(); self.push_tok(&mut t, &mut lines, start, Token::BitNot); }
                '"' | '\'' => {
                    self.adv();
                    let s = self.read_str(ch)?;
                    self.push_tok(&mut t, &mut lines, start, Token::StringLit(s));
                }
                '`' => {
                    self.adv();
                    let tpl = self.read_template()?;
                    self.push_tok(&mut t, &mut lines, start, tpl);
                }
                c if c.is_ascii_digit() => {
                    self.adv();
                    let num = self.read_num(c)?;
                    self.push_tok(&mut t, &mut lines, start, num);
                }
                c if c.is_alphabetic() || c == '_' || c == '$' => {
                    self.adv();
                    let ident = self.read_ident(c);
                    self.push_tok(&mut t, &mut lines, start, ident);
                }
                _ => { self.adv(); }
            }
        }
        Ok(TokenStream { tokens: t, lines })
    }
}

struct Parser {
    tokens: Vec<Token>,
    lines: Vec<u32>,
    pos: usize,
}

impl Parser {
    fn new(ts: TokenStream) -> Self { Self { tokens: ts.tokens, lines: ts.lines, pos: 0 } }
    fn peek(&self) -> &Token { self.tokens.get(self.pos).unwrap_or(&Token::Eof) }
    fn advance(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        t
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let mut s = Vec::new();
        while !matches!(self.peek(), Token::Eof) { s.push(self.parse_stmt()?); }
        Ok(s)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, CompileError> {
        match self.peek().clone() {
            Token::Let | Token::Const | Token::Var => self.parse_var_decl(),
            Token::Function => {
                self.advance();
                self.parse_fn_decl(false)
            }
            Token::Async => {
                if matches!(self.tokens.get(self.pos + 1), Some(Token::Function)) {
                    self.advance(); // async
                    self.advance(); // function
                    self.parse_fn_decl(true)
                } else {
                    // `async x => ...` / `async () => ...` as an expression.
                    self.parse_expr_stmt()
                }
            }
            Token::Return => self.parse_return(),
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::Do => self.parse_do(),
            Token::For => self.parse_for(),
            Token::LBrace => self.parse_block(),
            Token::Import => self.parse_import(),
            Token::Break => {
                self.advance();
                if let Token::Ident(s) = self.peek().clone() {
                    self.advance();
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::BreakLabel(s))
                } else {
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::Break)
                }
            }
            Token::Continue => {
                self.advance();
                if let Token::Ident(s) = self.peek().clone() {
                    self.advance();
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::ContinueLabel(s))
                } else {
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::Continue)
                }
            }
            // `name: statement` — a labeled statement (loop labels can be
            // targeted by `break name` / `continue name`).
            Token::Ident(s) if matches!(self.tokens.get(self.pos + 1), Some(Token::Colon)) => {
                self.advance(); // ident
                self.advance(); // colon
                let body = self.parse_stmt()?;
                Ok(Stmt::Labeled { name: s, body: Box::new(body) })
            }
            Token::Throw => {
                self.advance();
                let e = self.parse_expr(0)?;
                if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                Ok(Stmt::Throw(e))
            }
            Token::Try => self.parse_try(),
            Token::Switch => self.parse_switch(),
            Token::Export => self.parse_export(),
            Token::Class => self.parse_class_decl(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `export` declarations (module files loaded with `require`):
    ///   export let a = 1, b = 2;
    ///   export function f() {}
    ///   export async function g() {}
    ///   export { a, b };
    ///   export { a as c };
    ///   export default expr;
    /// Pairs are (public name, source binding); aliases differ in the two.
    /// The emitter records them in `program.exports`; the declaration itself
    /// compiles normally.
    fn parse_export(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // export
        // `export default expr` — stored under the reserved name `\0default`.
        if matches!(self.peek(), Token::Default) {
            self.advance();
            let e = self.parse_expr(0)?;
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
            return Ok(Stmt::Export {
                pairs: vec![("default".to_string(), DEFAULT_EXPORT.to_string())],
                stmt: Box::new(Stmt::Expr(e)),
                default: true,
            });
        }
        // `export { a, b as c }` — re-export existing bindings under names.
        if matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut pairs = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                match self.advance() {
                    Token::Ident(s) => {
                        let binding = s;
                        let name = if matches!(self.peek(), Token::As) {
                            self.advance();
                            match self.advance() {
                                Token::Ident(a) => a,
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!(
                                        "{:?}",
                                        t
                                    )))
                                }
                            }
                        } else {
                            binding.clone()
                        };
                        pairs.push((name, binding));
                    }
                    Token::Comma => {}
                    Token::Eof => {
                        return Err(CompileError::UnexpectedToken("RBrace".to_string()))
                    }
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if matches!(self.peek(), Token::RBrace) { self.advance(); }
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
            return Ok(Stmt::Export {
                pairs,
                stmt: Box::new(Stmt::Nop),
                default: false,
            });
        }
        // `export <declaration>`: let/const/var, function, async function.
        let (stmt, names) = match self.peek().clone() {
            Token::Let | Token::Const | Token::Var => {
                let s = self.parse_var_decl()?;
                let names = match &s {
                    Stmt::VarDecl { decls } => decls
                        .iter()
                        .flat_map(|(p, _)| pat_names(p))
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                };
                (s, names)
            }
            Token::Function => {
                self.advance();
                let s = self.parse_fn_decl(false)?;
                let names = match &s {
                    Stmt::FnDecl { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                };
                (s, names)
            }
            Token::Async
                if matches!(self.tokens.get(self.pos + 1), Some(Token::Function)) =>
            {
                self.advance(); // async
                self.advance(); // function
                let s = self.parse_fn_decl(true)?;
                let names = match &s {
                    Stmt::FnDecl { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                };
                (s, names)
            }
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        Ok(Stmt::Export {
            pairs: names.into_iter().map(|n| (n.clone(), n)).collect(),
            stmt: Box::new(stmt),
            default: false,
        })
    }

    fn parse_var_decl(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let mut decls = Vec::new();
        loop {
            let pat = self.parse_pattern()?;
            let init = if matches!(self.peek(), Token::Assign) {
                self.advance();
                // Initializers are AssignmentExpressions: the comma between
                // declarators (`let a = 1, b = 2`) stays a separator.
                Some(self.parse_expr(1)?)
            } else { None };
            decls.push((pat, init));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::VarDecl { decls })
    }

    /// Parse a binding target: a plain name, or a destructuring pattern
    /// `{ a, b: c }` / `[x, , y]` with nested patterns.
    fn parse_pattern(&mut self) -> Result<Pat, CompileError> {
        match self.peek().clone() {
            Token::Ident(s) => {
                self.advance();
                Ok(Pat::Bind(s))
            }
            Token::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                    if matches!(self.peek(), Token::DotDotDot) {
                        // `...rest` — must be the last element (a trailing
                        // comma after it is allowed).
                        self.advance();
                        let sub = self.parse_pattern()?;
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                        }
                        if !matches!(self.peek(), Token::RBrace) {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in object pattern".to_string(),
                            ));
                        }
                        fields.push(ObjPatElem::Rest(sub));
                        break;
                    }
                    if matches!(self.peek(), Token::LBracket) {
                        // `[expr]: v` — computed key, evaluated at runtime.
                        self.advance();
                        let key = self.parse_expr(0)?;
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        if !matches!(self.peek(), Token::Colon) {
                            return Err(CompileError::UnexpectedToken(
                                "expected ':' after computed key".to_string(),
                            ));
                        }
                        self.advance();
                        let sub = self.parse_pattern()?;
                        fields.push(ObjPatElem::Computed(key, sub));
                        match self.peek() {
                            Token::Comma => { self.advance(); }
                            Token::RBrace => {}
                            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                        }
                        continue;
                    }
                    // Constant keys are IdentifierNames: keywords are legal
                    // (`const { default: d } = o`).
                    let key = match self.advance() {
                        Token::StringLit(s) => s,
                        t => match keyword_text(&t) {
                            Some(s) => s,
                            None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                        },
                    };
                    let sub = if matches!(self.peek(), Token::Colon) {
                        self.advance();
                        self.parse_pattern()?
                    } else {
                        Pat::Bind(key.clone())
                    };
                    fields.push(ObjPatElem::Key(key, sub));
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBrace => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBrace) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Ok(Pat::Object(fields))
            }
            Token::LBracket => {
                self.advance();
                let mut elems = Vec::new();
                while !matches!(self.peek(), Token::RBracket | Token::Eof) {
                    if matches!(self.peek(), Token::Comma) {
                        elems.push(PatElem::Hole); // skip this element
                        self.advance();
                        continue;
                    }
                    if matches!(self.peek(), Token::DotDotDot) {
                        // `...rest` — must be the last element (a trailing
                        // comma after it is allowed).
                        self.advance();
                        let sub = self.parse_pattern()?;
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                        }
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in array pattern".to_string(),
                            ));
                        }
                        elems.push(PatElem::Rest(sub));
                        break;
                    }
                    elems.push(PatElem::Bind(self.parse_pattern()?));
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBracket => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Ok(Pat::Array(elems))
            }
            t => Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        }
    }

    /// `import` statements:
    ///   import { f } from 'alloy:core';          // builtins
    ///   import { f } from './x.py' as python;    // python sidecar module
    ///   import { f, g as h } from './x.ajs';     // sugar for require
    ///   import * as m from './x.ajs';            // whole exports object
    ///   import d from './x.ajs';                 // default export
    ///   import './x.ajs';                        // side effects only
    fn parse_import(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // import
        let kind = if matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut pairs = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                match self.advance() {
                    Token::Ident(s) => {
                        let exported = s;
                        let local = if matches!(self.peek(), Token::As) {
                            self.advance();
                            match self.advance() {
                                Token::Ident(a) => a,
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!(
                                        "{:?}",
                                        t
                                    )))
                                }
                            }
                        } else {
                            exported.clone()
                        };
                        pairs.push((exported, local));
                    }
                    Token::Comma => {}
                    Token::Eof => {
                        return Err(CompileError::UnexpectedToken("RBrace".to_string()))
                    }
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if matches!(self.peek(), Token::RBrace) { self.advance(); }
            ImportKind::ModuleNamed(pairs)
        } else if matches!(self.peek(), Token::Star) {
            self.advance();
            if matches!(self.peek(), Token::As) { self.advance(); }
            match self.advance() {
                Token::Ident(s) => ImportKind::ModuleNamespace(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else if matches!(self.peek(), Token::Ident(_)) {
            match self.advance() {
                Token::Ident(s) => ImportKind::ModuleDefault(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else {
            ImportKind::ModuleSideEffect
        };
        // `import './x.ajs'` (side effects only) skips the `from` keyword;
        // every other form is `... from '<src>'`.
        let src = if matches!(self.peek(), Token::From) {
            self.advance();
            match self.advance() {
                Token::StringLit(s) => s,
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else {
            match self.advance() {
                Token::StringLit(s) => s,
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        };
        // `import { f } from './x.py' as python` — the module-object binding.
        let py_alias = if matches!(self.peek(), Token::As) {
            self.advance();
            match self.advance() {
                Token::Ident(s) => Some(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else { None };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        let kind = if src == "alloy:core" || src == "alloy:fs" {
            // Builtins: the braced names (or a `* as m` namespace) bind
            // directly as globals seeded with the native.
            match kind {
                ImportKind::ModuleNamed(pairs) => {
                    ImportKind::Core(pairs.into_iter().map(|(_, l)| l).collect())
                }
                ImportKind::ModuleNamespace(n) => ImportKind::Core(vec![n]),
                _ => return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek()))),
            }
        } else if src.ends_with(".py") {
            ImportKind::Python(py_alias.unwrap_or_else(|| match kind {
                ImportKind::ModuleNamed(pairs) => {
                    pairs.first().map(|(_, l)| l.clone()).unwrap_or_default()
                }
                _ => String::new(),
            }))
        } else {
            kind
        };
        Ok(Stmt::Import { src, kind })
    }

    /// Parse a function declaration; the `function` keyword (and, if
    /// `is_async`, the preceding `async`) has already been consumed.
    fn parse_fn_decl(&mut self, is_async: bool) -> Result<Stmt, CompileError> {
        let name = match self.advance() {
            Token::Ident(s) => s,
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        let params = self.parse_params()?;
        let body = self.parse_block()?;
        Ok(Stmt::FnDecl { name, params, body: Box::new(body), is_async })
    }

    /// True when the parenthesized group at the current position is followed
    /// by `=>`, i.e. it is an arrow parameter list rather than a grouping.
    fn looks_like_arrow_params(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos;
        while let Some(t) = self.tokens.get(i) {
            match t {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.tokens.get(i + 1), Some(Token::Arrow));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Arrow body: `=> expr` (implicit return) or `=> { ... }` (block).
    fn parse_arrow_body(&mut self) -> Result<Box<Stmt>, CompileError> {
        if matches!(self.peek(), Token::LBrace) {
            Ok(Box::new(self.parse_block()?))
        } else {
            // The implicit-return body is an AssignmentExpression, so a comma
            // ends it (`x => a, b` is `(x => a), b`, not `x => (a, b)`).
            let e = self.parse_expr(1)?;
            Ok(Box::new(Stmt::Return(Some(e))))
        }
    }

    fn parse_params(&mut self) -> Result<FnParams, CompileError> {
        if matches!(self.peek(), Token::LParen) { self.advance(); }
        let mut params = Vec::new();
        let mut rest = None;
        if !matches!(self.peek(), Token::RParen) {
            loop {
                if matches!(self.peek(), Token::DotDotDot) {
                    // `...rest` — the rest parameter must be the last one and
                    // a plain binding identifier (JS requires this).
                    self.advance();
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    };
                    if matches!(self.peek(), Token::Comma) {
                        return Err(CompileError::UnexpectedToken(
                            "rest parameter must be last".to_string(),
                        ));
                    }
                    params.push(ParamDef { pat: Pat::Bind(name), default: None });
                    rest = Some(params.len() - 1);
                    break;
                }
                let pat = self.parse_pattern()?;
                let default = if matches!(self.peek(), Token::Assign) {
                    self.advance();
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                params.push(ParamDef { pat, default });
                if !matches!(self.peek(), Token::Comma) { break; }
                self.advance();
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(FnParams { params, rest })
    }

    fn parse_return(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let e = if matches!(self.peek(), Token::Semicolon) || matches!(self.peek(), Token::RBrace) {
            None
        } else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::Return(e))
    }

    fn parse_if(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let cond = self.parse_paren()?;
        let then = self.parse_stmt()?;
        let els = if matches!(self.peek(), Token::Else) {
            self.advance();
            Some(Box::new(self.parse_stmt()?))
        } else { None };
        Ok(Stmt::If { cond, then: Box::new(then), els })
    }

    /// Parse a parenthesized expression. Used for grouping `(a + b)` and for
    /// `if`/`while`/`switch` conditions, where the parens are optional in this
    /// engine (`if x > 0 { }` parses like Node's `if (x > 0) { }`). The closer
    /// is only required when an opener was actually consumed, so a parenless
    /// condition followed by `{` still parses.
    fn parse_paren(&mut self) -> Result<Expr, CompileError> {
        let had_paren = matches!(self.peek(), Token::LParen);
        if had_paren { self.advance(); }
        let e = self.parse_expr(0)?;
        if had_paren {
            if !matches!(self.peek(), Token::RParen) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance();
        }
        Ok(e)
    }

    fn parse_while(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let cond = self.parse_paren()?;
        let body = self.parse_stmt()?;
        Ok(Stmt::While { cond, body: Box::new(body) })
    }

    fn parse_do(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // do
        let body = self.parse_stmt()?;
        if !matches!(self.peek(), Token::While) {
            return Err(CompileError::UnexpectedToken(
                "expected 'while' after do body".to_string(),
            ));
        }
        self.advance(); // while
        let cond = self.parse_paren()?;
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::DoWhile { cond, body: Box::new(body) })
    }

    fn parse_for(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        if matches!(self.peek(), Token::LParen) { self.advance(); }

        // Detect `for (let x of ...)`, `for (let [a, b] of ...)`,
        // `for ({ a, b } in ...)` headers before falling back to the C-style
        // form. The pattern is parsed optimistically and rolled back if it is
        // not followed by `of`/`in`.
        let saved = self.pos;
        let mut declared = false;
        if matches!(self.peek(), Token::Let | Token::Const | Token::Var) {
            declared = true;
            self.advance();
        }
        let pat = match self.parse_pattern() {
            Ok(p) => p,
            Err(_) => {
                self.pos = saved;
                Pat::Bind(String::new())
            }
        };
        if matches!(self.peek(), Token::Of | Token::In) {
            let is_in = matches!(self.peek(), Token::In);
            self.advance();
            // The iterable is an AssignmentExpression: `for (x of a, b)` is a
            // SyntaxError (the comma is neither an operator nor a separator
            // here), so parse at min_bp 1 and let the trailing comma fail.
            let source = self.parse_expr(1)?;
            if matches!(self.peek(), Token::RParen) { self.advance(); }
            let body = self.parse_stmt()?;
            if is_in {
                return Ok(Stmt::ForIn { pat, declared, obj: source, body: Box::new(body) });
            } else {
                return Ok(Stmt::ForOf { pat, declared, iterable: source, body: Box::new(body) });
            }
        }
        self.pos = saved;

        let init = if matches!(self.peek(), Token::Semicolon) {
            self.advance(); None
        } else if matches!(self.peek(), Token::Let) || matches!(self.peek(), Token::Const) {
            Some(Box::new(self.parse_var_decl()?))
        } else {
            Some(Box::new(self.parse_expr_stmt()?))
        };
        let cond = if matches!(self.peek(), Token::Semicolon) { None }
        else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        let update = if matches!(self.peek(), Token::RParen) { None }
        else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::RParen) { self.advance(); }
        let body = self.parse_stmt()?;
        Ok(Stmt::For { init, cond, update, body: Box::new(body) })
    }

    fn parse_try(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // try
        let body = self.parse_block()?;
        let mut catch = None;
        let mut finally = None;
        if matches!(self.peek(), Token::Catch) {
            self.advance();
            let name = if matches!(self.peek(), Token::LParen) {
                self.advance();
                match self.advance() {
                    Token::Ident(s) => s,
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            } else {
                String::new()
            };
            if matches!(self.peek(), Token::RParen) { self.advance(); }
            let block = self.parse_block()?;
            catch = Some((name, Box::new(block)));
        }
        if matches!(self.peek(), Token::Finally) {
            self.advance();
            let block = self.parse_block()?;
            finally = Some(Box::new(block));
        }
        if catch.is_none() && finally.is_none() {
            return Err(CompileError::UnexpectedToken("try without catch or finally".to_string()));
        }
        Ok(Stmt::Try { body: Box::new(body), catch, finally })
    }

    fn parse_switch(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // switch
        let disc = self.parse_paren()?;
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut cases = Vec::new();
        loop {
            match self.peek().clone() {
                Token::Case => {
                    self.advance();
                    let test = self.parse_expr(0)?;
                    if matches!(self.peek(), Token::Colon) { self.advance(); }
                    let mut body = Vec::new();
                    while !matches!(self.peek(), Token::Case | Token::Default | Token::RBrace | Token::Eof) {
                        body.push(self.parse_stmt()?);
                    }
                    cases.push(SwitchCase { test: Some(test), body });
                }
                Token::Default => {
                    self.advance();
                    if matches!(self.peek(), Token::Colon) { self.advance(); }
                    let mut body = Vec::new();
                    while !matches!(self.peek(), Token::Case | Token::Default | Token::RBrace | Token::Eof) {
                        body.push(self.parse_stmt()?);
                    }
                    cases.push(SwitchCase { test: None, body });
                }
                Token::RBrace => {
                    self.advance();
                    break;
                }
                Token::Eof => break,
                t => {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                }
            }
        }
        Ok(Stmt::Switch { disc, cases })
    }

    /// `class Name extends Parent { … }` — a declaration (name required).
    fn parse_class_decl(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // class
        let name = match self.advance() {
            Token::Ident(s) => s,
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        let extends = if matches!(self.peek(), Token::Extends) {
            self.advance();
            Some(self.parse_expr(1)?)
        } else {
            None
        };
        let methods = self.parse_class_body()?;
        Ok(Stmt::Class { name, extends, methods })
    }

    /// `class [Name] extends Parent { … }` — a class expression.
    fn parse_class_expr(&mut self) -> Result<Expr, CompileError> {
        self.advance(); // class
        let name = match self.peek() {
            Token::Ident(s) => {
                let s = s.clone();
                self.advance();
                Some(s)
            }
            _ => None,
        };
        let extends = if matches!(self.peek(), Token::Extends) {
            self.advance();
            Some(Box::new(self.parse_expr(1)?))
        } else {
            None
        };
        let methods = self.parse_class_body()?;
        Ok(Expr::Class { name, extends, methods })
    }

    /// `{ [static] [async] name(params) { body } … }` — the class body. The
    /// constructor is the method named `constructor`.
    fn parse_class_body(&mut self) -> Result<Vec<MethodDef>, CompileError> {
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut methods = Vec::new();
        while !matches!(self.peek(), Token::RBrace) && !matches!(self.peek(), Token::Eof) {
            let mut is_static = false;
            if matches!(self.peek(), Token::Static) {
                self.advance();
                is_static = true;
            }
            let mut is_async = false;
            if matches!(self.peek(), Token::Async)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(_)))
            {
                self.advance();
                is_async = true;
            }
            // Method names are IdentifierNames: any keyword is legal
            // (`constructor`, `get`, `default`…).
            let name = match self.advance() {
                Token::Ident(s) => s,
                Token::StringLit(s) => s,
                t => match keyword_text(&t) {
                    Some(s) => s,
                    None => {
                        return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                    }
                },
            };
            if !matches!(self.peek(), Token::LParen) {
                return Err(CompileError::UnexpectedToken(
                    "expected '(' after method name (getters/setters/fields not supported)".to_string(),
                ));
            }
            self.advance(); // (
            let params = self.parse_params()?;
            let body = Box::new(self.parse_block()?);
            methods.push(MethodDef { name, is_static, is_async, kind: MethodKind::Normal, params, body, init: None });
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        }
        if !matches!(self.peek(), Token::RBrace) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(methods)
    }

    /// `new C(args)` / `new C.m(args)`: parse the callee as a member chain
    /// (no call parens — those are the constructor arguments) plus optional
    /// argument list. `new C` with no parens means `new C()`.
    fn parse_new_expr(&mut self) -> Result<Expr, CompileError> {
        self.advance(); // new
        let mut callee = match self.peek().clone() {
            Token::Ident(s) => {
                self.advance();
                Expr::Ident(s)
            }
            Token::This => {
                self.advance();
                Expr::Ident("this".into())
            }
            Token::LParen => {
                // `new (factory())()` — parenthesized constructor expression.
                let e = self.parse_paren()?;
                e
            }
            Token::LBracket | Token::Function | Token::Async => self.parse_expr(14)?,
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        // Member chain: `.prop` / `[idx]` (no call parens yet).
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    let prop = match self.advance() {
                        t => match keyword_text(&t) {
                            Some(s) => s,
                            None => {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                            }
                        },
                    };
                    callee = Expr::Prop { obj: Box::new(callee), prop, optional: false };
                }
                Token::LBracket => {
                    self.advance();
                    let idx = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RBracket) {
                        return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                    }
                    self.advance();
                    callee = Expr::Index { obj: Box::new(callee), index: Box::new(idx), optional: false };
                }
                _ => break,
            }
        }
        let mut args = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            while !matches!(self.peek(), Token::RParen) {
                let spread = matches!(self.peek(), Token::DotDotDot);
                if spread { self.advance(); }
                args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                match self.peek() {
                    Token::Comma => { self.advance(); }
                    Token::RParen => {}
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance();
        }
        Ok(Expr::New { callee: Box::new(callee), args })
    }

    fn parse_block(&mut self) -> Result<Stmt, CompileError> {
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut s = Vec::new();
        while !matches!(self.peek(), Token::RBrace) && !matches!(self.peek(), Token::Eof) {
            s.push(self.parse_stmt()?);
        }
        if !matches!(self.peek(), Token::RBrace) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(Stmt::Block(s))
    }

    fn parse_expr_stmt(&mut self) -> Result<Stmt, CompileError> {
        let e = self.parse_expr(0)?;
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::Expr(e))
    }

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, CompileError> {
        // Fresh expression boundary: parens, statement contexts, list
        // elements, and operand parses all reset the `??`/`&&`/`||` mixing
        // family. Only the binary-op RHS recursion (below) threads it.
        self.parse_expr_inner(min_bp, None)
    }

    /// `family` is the nullish/logical family already in use at this parse
    /// level: `Some("nullish")` after `??`, `Some("logical")` after
    /// `&&`/`||`. JS forbids mixing the two in one unparenthesized
    /// expression (`a ?? b || c` and `a && b ?? c` are SyntaxErrors), so an
    /// op whose family differs from the incoming one is rejected.
    fn parse_expr_inner(
        &mut self,
        min_bp: u8,
        mut family: Option<&'static str>,
    ) -> Result<Expr, CompileError> {
        let mut lhs = match self.peek().clone() {
            Token::Number(n) => { self.advance(); Expr::Num(n) }
            Token::Int(i) => { self.advance(); Expr::Int(i) }
            Token::StringLit(s) => { self.advance(); Expr::Str(s) }
            Token::True => { self.advance(); Expr::Bool(true) }
            Token::False => { self.advance(); Expr::Bool(false) }
            Token::Null => { self.advance(); Expr::Null }
            Token::Undefined => { self.advance(); Expr::Undef }
            Token::Regex { pattern, flags } => {
                self.advance();
                Expr::Regex { pattern, flags }
            }
            Token::Ident(s) => { self.advance(); Expr::Ident(s) }
            Token::Minus => {
                self.advance();
                Expr::Unary("-", Box::new(self.parse_expr(15)?))
            }
            Token::Not => {
                self.advance();
                Expr::Unary("!", Box::new(self.parse_expr(15)?))
            }
            Token::PlusPlus => {
                self.advance();
                Expr::IncDec { target: Box::new(self.parse_expr(15)?), is_inc: true, is_prefix: true }
            }
            Token::MinusMinus => {
                self.advance();
                Expr::IncDec { target: Box::new(self.parse_expr(15)?), is_inc: false, is_prefix: true }
            }
            Token::Typeof => {
                self.advance();
                Expr::Unary("typeof", Box::new(self.parse_expr(15)?))
            }
            Token::Void => {
                self.advance();
                Expr::Unary("void", Box::new(self.parse_expr(15)?))
            }
            Token::Delete => {
                self.advance();
                Expr::Delete(Box::new(self.parse_expr(15)?))
            }
            Token::BitNot => {
                self.advance();
                Expr::Unary("~", Box::new(self.parse_expr(15)?))
            }
            Token::Await => {
                self.advance();
                Expr::Await(Box::new(self.parse_expr(15)?))
            }
            Token::This => {
                self.advance();
                Expr::Ident("this".into())
            }
            Token::New => {
                let e = self.parse_new_expr()?;
                e
            }
            Token::Super => {
                self.advance();
                match self.peek() {
                    Token::Dot => {
                        self.advance();
                        let prop = match self.advance() {
                            t => match keyword_text(&t) {
                                Some(s) => s,
                                None => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            },
                        };
                        let args = if matches!(self.peek(), Token::LParen) {
                            self.advance();
                            let mut args = Vec::new();
                            while !matches!(self.peek(), Token::RParen) {
                                let spread = matches!(self.peek(), Token::DotDotDot);
                                if spread { self.advance(); }
                                args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                                match self.peek() {
                                    Token::Comma => { self.advance(); }
                                    Token::RParen => {}
                                    t => {
                                        return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                    }
                                }
                            }
                            if !matches!(self.peek(), Token::RParen) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            Some(args)
                        } else {
                            None
                        };
                        Expr::SuperProp { prop, args }
                    }
                    Token::LParen => {
                        self.advance();
                        let mut args = Vec::new();
                        while !matches!(self.peek(), Token::RParen) {
                            let spread = matches!(self.peek(), Token::DotDotDot);
                            if spread { self.advance(); }
                            args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                            match self.peek() {
                                Token::Comma => { self.advance(); }
                                Token::RParen => {}
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            }
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        Expr::SuperCall { args }
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken(
                            "super must be followed by '(' or '.m('".to_string(),
                        ));
                    }
                }
            }
            Token::Class => {
                let e = self.parse_class_expr()?;
                e
            }
            Token::Async => {
                self.advance();
                if matches!(self.peek(), Token::Function) {
                    self.advance();
                    let p = self.parse_params()?;
                    let b = self.parse_block()?;
                    Expr::Lambda { params: p, body: Box::new(b), is_async: true, is_arrow: false }
                } else if matches!(self.peek(), Token::LParen) && self.looks_like_arrow_params() {
                    self.advance(); // (
                    let params = self.parse_params()?;
                    self.advance(); // =>
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda { params, body, is_async: true, is_arrow: true }
                } else if matches!(self.peek(), Token::Ident(_)) {
                    // `async x => body`
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    if matches!(self.peek(), Token::Arrow) { self.advance(); }
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda {
                        params: FnParams { params: vec![ParamDef { pat: Pat::Bind(name), default: None }], rest: None },
                        body,
                        is_async: true,
                        is_arrow: true,
                    }
                } else {
                    return Err(CompileError::UnexpectedToken("async without function/arrow".to_string()));
                }
            }
            Token::LParen => {
                if self.looks_like_arrow_params() {
                    self.advance(); // (
                    let params = self.parse_params()?;
                    self.advance(); // =>
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda { params, body, is_async: false, is_arrow: true }
                } else {
                    self.advance();
                    let e = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                    }
                    self.advance();
                    Expr::Paren(Box::new(e))
                }
            }
            Token::TemplateLit(parts) => {
                self.advance();
                Expr::Template(parts)
            }
            Token::LBracket => {
                self.advance();
                let mut el = Vec::new();
                while !matches!(self.peek(), Token::RBracket) {
                    if matches!(self.peek(), Token::Comma) {
                        // Elision `[a, , b]`: an empty slot that evaluates to
                        // undefined (and maps to a hole in assignment targets).
                        el.push(Elem { spread: false, hole: true, expr: Expr::Undef });
                        self.advance();
                        continue;
                    }
                    let spread = matches!(self.peek(), Token::DotDotDot);
                    if spread { self.advance(); }
                    // Elements are AssignmentExpressions; the comma is the
                    // list separator, not the operator.
                    el.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                    // Commas are REQUIRED between elements (`[1 2]` is a
                    // SyntaxError in JS, not `[1, 2]`).
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBracket => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Expr::Array(el)
            }
            Token::Function => {
                self.advance();
                let p = self.parse_params()?;
                let b = self.parse_block()?;
                Expr::Lambda { params: p, body: Box::new(b), is_async: false, is_arrow: false }
            }
            Token::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Token::RBrace) {
                    match self.peek().clone() {
                        // `{ ...expr }`: copy the source's own enumerable
                        // properties, in order. `{...null}` is a no-op.
                        Token::DotDotDot => {
                            self.advance();
                            let e = self.parse_expr(1)?;
                            fields.push(ObjElem::Spread(e));
                        }
                        // `{ [expr]: value }`: the key is evaluated at
                        // runtime and coerced to a string.
                        Token::LBracket => {
                            self.advance();
                            let k = self.parse_expr(0)?;
                            if !matches!(self.peek(), Token::RBracket) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            if !matches!(self.peek(), Token::Colon) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            let v = self.parse_expr(1)?;
                            fields.push(ObjElem::Computed(k, v));
                        }
                        // `{ async f() {} }` — async method shorthand.
                        Token::Async
                            if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(_)))
                                && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen)) =>
                        {
                            self.advance(); // async
                            let name = match self.advance() {
                                Token::Ident(s) => s,
                                _ => unreachable!(),
                            };
                            let params = self.parse_params()?;
                            let b = self.parse_block()?;
                            fields.push(ObjElem::Pair(
                                name,
                                Expr::Lambda {
                                    params,
                                    body: Box::new(b),
                                    is_async: true,
                                    is_arrow: false,
                                },
                            ));
                        }
                        _ => {
                            // Keys are IdentifierNames: any keyword is legal
                            // (`{ default: 1, if: 2 }`), plus string keys.
                            let key = match self.advance() {
                                Token::StringLit(s) => s,
                                t => match keyword_text(&t) {
                                    Some(s) => s,
                                    None => {
                                        return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                    }
                                },
                            };
                            if matches!(self.peek(), Token::LParen) {
                                // Method shorthand `{ f(a) { ... } }`: a
                                // non-arrow lambda, so `this` binds via the
                                // receiver when called as `o.f()`.
                                self.advance();
                                let params = self.parse_params()?;
                                let b = self.parse_block()?;
                                fields.push(ObjElem::Pair(
                                    key,
                                    Expr::Lambda {
                                        params,
                                        body: Box::new(b),
                                        is_async: false,
                                        is_arrow: false,
                                    },
                                ));
                            } else if matches!(self.peek(), Token::Colon) {
                                self.advance();
                                // Property values are AssignmentExpressions;
                                // the comma is the field separator — and it
                                // is REQUIRED between fields (`{a: 1 b: 2}`
                                // is a SyntaxError in JS).
                                let value = self.parse_expr(1)?;
                                fields.push(ObjElem::Pair(key, value));
                            } else {
                                // Shorthand `{ a }` — the value is the
                                // identifier itself. This is the only legal
                                // no-colon form; `{ a.b }` errors (shorthand
                                // must be an IdentifierReference in JS).
                                fields.push(ObjElem::Pair(key.clone(), Expr::Ident(key)));
                            }
                        }
                    }
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBrace => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBrace) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Expr::Object(fields)
            }
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };

        loop {
            if matches!(self.peek(), Token::LParen) {
                self.advance();
                let mut args = Vec::new();
                while !matches!(self.peek(), Token::RParen) {
                    let spread = matches!(self.peek(), Token::DotDotDot);
                    if spread { self.advance(); }
                    // Arguments are AssignmentExpressions; the comma is the
                    // list separator, not the operator.
                    args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                    // Commas are REQUIRED between arguments — `f(a b)` is a
                    // SyntaxError in JS, and a missing comma is how `0.0
                    // toString()` used to silently become two arguments.
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RParen => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RParen) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                lhs = Expr::Call { callee: Box::new(lhs), args, optional: false };
                continue;
            } else if matches!(self.peek(), Token::Dot) {
                self.advance();
                // Reserved words are legal property names after a dot
                // (`m.default`, `o.delete`, `o.if` — the IdentifierName
                // rule). Any keyword token is mapped back to its text.
                let prop = match self.advance() {
                    t => match keyword_text(&t) {
                        Some(s) => s,
                        None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    },
                };
                lhs = Expr::Prop { obj: Box::new(lhs), prop, optional: false };
                continue;
            } else if matches!(self.peek(), Token::QuestionDot) {
                // Optional chaining: `o?.p`, `o?.[k]`, `f?.()`. The `?.` must
                // be followed by a property name, `[`, or `(` (the lexer
                // already reclassifies `?.5` as a ternary). Each link records
                // its optionality; the emitter short-circuits the whole
                // remaining chain when the link's receiver is nullish.
                self.advance(); // ?.
                match self.peek() {
                    Token::LBracket => {
                        self.advance();
                        let idx = self.parse_expr(0)?;
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        lhs = Expr::Index {
                            obj: Box::new(lhs),
                            index: Box::new(idx),
                            optional: true,
                        };
                    }
                    Token::LParen => {
                        self.advance();
                        let mut args = Vec::new();
                        while !matches!(self.peek(), Token::RParen) {
                            let spread = matches!(self.peek(), Token::DotDotDot);
                            if spread {
                                self.advance();
                            }
                            args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                            match self.peek() {
                                Token::Comma => {
                                    self.advance();
                                }
                                Token::RParen => {}
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            }
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        lhs = Expr::Call {
                            callee: Box::new(lhs),
                            args,
                            optional: true,
                        };
                    }
                    // `?.prop` / `?.keyword` (reserved words are legal
                    // property names after `?.`, same as after `.`).
                    t => match keyword_text(&t) {
                        Some(s) => {
                            self.advance();
                            lhs = Expr::Prop {
                                obj: Box::new(lhs),
                                prop: s,
                                optional: true,
                            };
                        }
                        None => {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                    },
                }
                continue;
            } else if matches!(self.peek(), Token::LBracket) {
                self.advance();
                let idx = self.parse_expr(0)?;
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                lhs = Expr::Index { obj: Box::new(lhs), index: Box::new(idx), optional: false };
                continue;
            } else if matches!(self.peek(), Token::Arrow) && matches!(&lhs, Expr::Ident(_)) {
                // Single-parameter arrow shorthand: `x => body`.
                let name = match &lhs {
                    Expr::Ident(s) => s.clone(),
                    _ => unreachable!(),
                };
                self.advance(); // =>
                let body = self.parse_arrow_body()?;
                lhs = Expr::Lambda {
                    params: FnParams { params: vec![ParamDef { pat: Pat::Bind(name), default: None }], rest: None },
                    body,
                    is_async: false,
                    is_arrow: true,
                };
                continue;
            } else if matches!(self.peek(), Token::PlusPlus | Token::MinusMinus) {
                // Postfix `x++` / `x--`; binds tighter than any binary op.
                let is_inc = matches!(self.peek(), Token::PlusPlus);
                self.advance();
                lhs = Expr::IncDec { target: Box::new(lhs), is_inc, is_prefix: false };
                continue;
            }
            let (op, bp): (&str, u8) = match self.peek() {
                Token::Plus => ("+", 10),
                Token::Minus => ("-", 10),
                Token::Star => ("*", 11),
                Token::Slash => ("/", 11),
                Token::Percent => ("%", 11),
                Token::StarStar => {
                    // `**` binds tighter than `*`/`/`/`%` and is
                    // right-associative. JS restricts the LEFT operand to a
                    // non-unary expression (`-2 ** 2` is a SyntaxError; the
                    // exponent itself may be unary: `2 ** -2` is fine).
                    if matches!(lhs, Expr::Unary(..)) {
                        return Err(CompileError::UnexpectedToken(
                            "unary expression cannot be the left operand of '**'".to_string(),
                        ));
                    }
                    ("**", 12)
                }
                Token::EqEq => ("==", 7),
                Token::Neq => ("!=", 7),
                Token::EqEqEq => ("===", 7),
                Token::NeqEq => ("!==", 7),
                Token::Lt => ("<", 8),
                Token::Gt => (">", 8),
                Token::Lte => ("<=", 8),
                Token::Gte => (">=", 8),
                // `instanceof` and the `in` operator bind at relational
                // precedence (JS spec).
                Token::InstanceOf => ("instanceof", 8),
                Token::In => ("in", 8),
                // Shifts bind between relational and additive: `a << b + c`
                // is `a << (b + c)`, `a < b << c` is `a < (b << c)`.
                Token::Shl => ("<<", 9),
                Token::Shr => (">>", 9),
                Token::UShr => (">>>", 9),
                Token::And => ("&&", 3),
                Token::Or => ("||", 2),
                Token::QuestionQuestion => ("??", 2),
                // Bitwise ops bind between the logical ops and equality,
                // matching JS precedence: `||` < `&&` < `|` < `^` < `&` < eq.
                Token::BitOr => ("|", 4),
                Token::BitXor => ("^", 5),
                Token::BitAnd => ("&", 6),
                Token::Assign => ("=", 1),
                Token::PlusAssign => ("+=", 1),
                Token::MinusAssign => ("-=", 1),
                Token::StarAssign => ("*=", 1),
                Token::SlashAssign => ("/=", 1),
                Token::PercentAssign => ("%=", 1),
                Token::BitAndAssign => ("&=", 1),
                Token::BitOrAssign => ("|=", 1),
                Token::BitXorAssign => ("^=", 1),
                Token::ShlAssign => ("<<=", 1),
                Token::ShrAssign => (">>=", 1),
                Token::UShrAssign => (">>>=", 1),
                Token::StarStarAssign => ("**=", 1),
                Token::AndAssign => ("&&=", 1),
                Token::OrAssign => ("||=", 1),
                Token::NullishAssign => ("??=", 1),
                // No binary operator here. Two expressions directly adjacent
                // (no operator, no postfix) is invalid JS (`0.toString` is a
                // SyntaxError in Node because `0.` lexes as a number and the
                // identifier follows it; `a b` and `a 5` are errors too).
                // This used to silently misparse — the dangling identifier
                // became a second call argument or a second statement.
                // Only a SAME-LINE adjacency is an error. The engine treats a
                // newline as an implicit statement separator (tests/REPL rely
                // on `const f = x => x` \n `print(f())` with no semicolon).
                t if starts_expression(t)
                    && self.pos > 0
                    && self.lines[self.pos] == self.lines[self.pos - 1] =>
                {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                _ => break,
            };
            if bp < min_bp { break; }
            // `??` cannot be mixed with `&&`/`||` in one unparenthesized
            // expression (JS SyntaxError). The family of the incoming parse
            // level flows down through the RHS recursion, so the check fires
            // regardless of nesting order (`a ?? b && c`, `a && b ?? c`,
            // `a ?? b || c`); parens reset it via the parse_expr wrapper.
            let fam = match op {
                "??" => Some("nullish"),
                "&&" | "||" => Some("logical"),
                _ => None,
            };
            let next_family = match (family, fam) {
                (Some(a), Some(b)) if a != b => {
                    return Err(CompileError::UnexpectedToken(
                        "mix of '??' with '&&' or '||' requires parentheses".to_string(),
                    ));
                }
                (_, Some(b)) => Some(b),
                (f, None) => f,
            };
            self.advance();
            // Assignment operators and `**` are right-associative: their RHS
            // parses at the same precedence, so `a = b = c` chains into
            // `a = (b = c)` and `2 ** 3 ** 2` into `2 ** (3 ** 2)`.
            let is_assign = op == "=" || op == "+=" || op == "-=" || op == "*=" || op == "/="
                || op == "%=" || op == "&=" || op == "|=" || op == "^=" || op == "<<="
                || op == ">>=" || op == ">>>=" || op == "**=" || op == "&&=" || op == "||="
                || op == "??=";
            let right_assoc = is_assign || op == "**";
            let rhs = self.parse_expr_inner(if right_assoc { bp } else { bp + 1 }, next_family)?;
            lhs = if is_assign {
                Expr::Assign { target: Box::new(lhs), op, value: Box::new(rhs) }
            } else {
                Expr::Bin(op, Box::new(lhs), Box::new(rhs))
            };
            // The family persists across the loop's remaining iterations, so
            // a later same-precedence op sees it: `a ?? b || c` (both bp 2,
            // left-assoc) must still hit the mixing rule even though the RHS
            // recursion stopped before the `||`.
            family = next_family;
            continue;
        }

        // Ternary `cond ? then : else`. Binds looser than `||` (so it is not
        // consumed as the RHS of `||`, min_bp 3) but tighter than assignment
        // (so `x = a ? b : c` takes the whole ternary as the RHS, min_bp 2).
        // The branches are AssignmentExpressions (JS grammar), so they parse
        // at min_bp 1 — assignments allowed, the comma operator is not (it
        // needs parens): `a ? b : c, d` is `(a ? b : c), d`.
        if matches!(self.peek(), Token::Question) {
            if 2 < min_bp {
                return Ok(lhs);
            }
            self.advance(); // ?
            let then_branch = self.parse_expr(1)?;
            if !matches!(self.peek(), Token::Colon) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance(); // :
            let else_branch = self.parse_expr(1)?;
            lhs = Expr::Ternary {
                cond: Box::new(lhs),
                then: Box::new(then_branch),
                els: Box::new(else_branch),
            };
        }

        // Comma operator: the lowest-precedence expression. Only a "full
        // expression" context (min_bp == 0 — parens, statements, return,
        // conditions, for-init/update, template holes, switch tests) sees it;
        // separator contexts parse at min_bp >= 1 so `f(a, b)` / `[a, b]` /
        // `let x = 1, y = 2` keep their commas as separators. Each element is
        // itself a full expression minus the comma (min_bp 1, so assignments
        // and ternaries are allowed inside).
        if min_bp == 0 && matches!(self.peek(), Token::Comma) {
            let mut seq = vec![lhs];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                seq.push(self.parse_expr(1)?);
            }
            lhs = Expr::Sequence(seq);
        }
        Ok(lhs)
    }
}

#[derive(Debug, Clone)]
enum Expr {
    Num(f64),
    Int(i64),
    Str(String),
    Bool(bool),
    Null,
    Undef,
    Ident(String),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    /// `delete target`: member expressions delete the property/index, a plain
    /// identifier evaluates to false, anything else evaluates and yields true.
    Delete(Box<Expr>),
    Assign { target: Box<Expr>, op: &'static str, value: Box<Expr> },
    /// A call. `optional` is true when written `f?.()` — the args are not
    /// evaluated and the whole chain yields undefined when the callee is
    /// nullish.
    Call { callee: Box<Expr>, args: Vec<Elem>, optional: bool },
    /// A property read. `optional` is true when written `o?.p`.
    Prop { obj: Box<Expr>, prop: String, optional: bool },
    /// An index read. `optional` is true when written `o?.[k]`.
    Index { obj: Box<Expr>, index: Box<Expr>, optional: bool },
    Array(Vec<Elem>),
    Object(Vec<ObjElem>),
    Template(Vec<TemplatePart>),
    Lambda { params: FnParams, body: Box<Stmt>, is_async: bool, is_arrow: bool },
    Await(Box<Expr>),
    Ternary { cond: Box<Expr>, then: Box<Expr>, els: Box<Expr> },
    IncDec { target: Box<Expr>, is_inc: bool, is_prefix: bool },
    /// Grouping parens `(expr)`: a thin wrapper the emitter unwraps. It exists
    /// so `(-2) ** 2` (a parenthesized unary, legal as the left operand of
    /// `**`) is distinguishable from the bare `-2 ** 2` SyntaxError.
    Paren(Box<Expr>),
    /// The comma operator `(a, b, c)`: evaluate each element left-to-right,
    /// the expression's value is the last one. Only parsed in full-expression
    /// contexts (parens, statements, return, conds); in argument/array/object/
    /// declarator lists the comma stays a separator.
    Sequence(Vec<Expr>),
    /// `new C(args)` — allocate an instance (proto = `C.prototype`) and call
    /// the constructor with `this` bound to it.
    New { callee: Box<Expr>, args: Vec<Elem> },
    /// `super(args)` inside a derived class's constructor: call the parent
    /// constructor with the current `this`.
    SuperCall { args: Vec<Elem> },
    /// `super.m(args)` inside a method: look `m` up on the method's home
    /// object's parent prototype and call it with the current `this`.
    /// `args: None` is a bare `super.m` reference (no call).
    SuperProp { prop: String, args: Option<Vec<Elem>> },
    /// `class Name extends Parent { … }` / `class extends Parent { … }` —
    /// evaluates to the class (constructor) value.
    Class { name: Option<String>, extends: Option<Box<Expr>>, methods: Vec<MethodDef> },
    /// `/pattern/flags` — a fresh regex object per evaluation (its own
    /// `lastIndex`), like JS.
    Regex { pattern: String, flags: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodKind {
    Normal,
    Getter,
    Setter,
    Field,
}

#[derive(Debug, Clone)]
struct MethodDef {
    name: String,
    is_static: bool,
    is_async: bool,
    kind: MethodKind,
    params: FnParams,
    body: Box<Stmt>,
    init: Option<Expr>,
}

/// One element of an array literal or call argument list; `spread` marks
/// `...e` and `hole` marks an elision (`[a, , b]`), which evaluates to
/// `undefined` in a literal and skips a position in an assignment target.
#[derive(Debug, Clone)]
struct Elem {
    spread: bool,
    hole: bool,
    expr: Expr,
}

/// Where a logical assignment (`&&=`, `||=`, `??=`) writes its result:
/// a named slot, or a member whose receiver (and index) were stashed in
/// fresh locals before the RHS ran.
#[derive(Clone, Copy)]
enum LStore {
    Local(u8),
    Upvalue(u8),
    Global(u16),
    /// (constant index of the property name, receiver temp slot)
    Prop(u16, u8),
    /// (receiver temp slot, index temp slot)
    Index(u8, u8),
}

/// One element of an object literal. `Pair` is a constant-key property;
/// `Computed` is `[expr]: value` (the key is evaluated at runtime and
/// coerced to a string); `Spread` is `...expr` (the source's own enumerable
/// properties are copied in order). Method shorthand `{ f() {} }` is parsed
/// into a `Pair` whose value is a non-arrow `Lambda`, so `this` binds via the
/// receiver like any method call.
#[derive(Debug, Clone)]
enum ObjElem {
    Pair(String, Expr),
    Computed(Expr, Expr),
    Spread(Expr),
}

/// One position in an array destructuring pattern: a hole skips an element, a
/// `Bind` reads one element, and the (last) `Rest` element collects the
/// remainder of the source into an array.
#[derive(Debug, Clone)]
enum PatElem {
    Hole,
    Bind(Pat),
    Rest(Pat),
}

/// One element of an OBJECT destructuring pattern: a constant key, a
/// computed key (`[expr]: v` — the key expression is evaluated at runtime),
/// or the (last) `Rest` element (`...rest` — the source's remaining own
/// enumerable properties).
#[derive(Debug, Clone)]
enum ObjPatElem {
    Key(String, Pat),
    Computed(Expr, Pat),
    Rest(Pat),
}

/// A destructuring pattern: `Bind(name)` binds a variable; `Object` matches
/// keys via `GetProperty`; `Array` matches indexes via `GetIndex`.
#[derive(Debug, Clone)]
enum Pat {
    Bind(String),
    Object(Vec<ObjPatElem>),
    Array(Vec<PatElem>),
}

/// One parameter: a binding pattern plus an optional default value
/// (`function f(a = 1, { b } = {}) {}` — the default is used when the
/// argument is `undefined`).
#[derive(Debug, Clone)]
struct ParamDef {
    pat: Pat,
    default: Option<Expr>,
}

/// A function's parameter list: one [`ParamDef`] per position (the rest
/// parameter, if any, is the last position — a plain `Bind`).
#[derive(Debug, Clone)]
struct FnParams {
    params: Vec<ParamDef>,
    rest: Option<usize>,
}

impl FnParams {
    /// The callee-frame local layout: one slot per parameter position
    /// (a `Bind` param IS its slot; a pattern param occupies a synthetic
    /// `\0param{i}` slot holding the raw argument), then the pattern-bound
    /// names, then — last — the rest param's slot.
    fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, p) in self.params.iter().enumerate() {
            match &p.pat {
                Pat::Bind(n) => out.push(n.clone()),
                _ => out.push(format!("\u{0}param{}", i)),
            }
        }
        // Pattern params add their bound names; Bind params were already
        // pushed above — counting them again double-allocated the slot and
        // made `resolve`'s rposition hand back the duplicate (param read
        // one slot past the real argument).
        for p in &self.params {
            if matches!(p.pat, Pat::Bind(_)) {
                continue;
            }
            pat_bound_names(&p.pat, &mut out);
        }
        out
    }

    /// Fixed (non-rest) parameter count — what the VM pre-fills with
    /// undefined on a short call.
    fn pcount(&self) -> usize {
        self.params.len() - usize::from(self.rest.is_some())
    }

    /// The rest param's slot index (always the last local).
    fn rest_slot(&self) -> usize {
        self.names().len() - 1
    }
}

/// All names bound by a pattern, in order (used to lay out locals).
fn pat_bound_names(p: &Pat, out: &mut Vec<String>) {
    match p {
        Pat::Bind(n) => out.push(n.clone()),
        Pat::Object(elems) => {
            for el in elems {
                match el {
                    ObjPatElem::Key(_, sub) | ObjPatElem::Computed(_, sub) | ObjPatElem::Rest(sub) => {
                        pat_bound_names(sub, out)
                    }
                }
            }
        }
        Pat::Array(elems) => {
            for el in elems {
                match el {
                    PatElem::Bind(sub) | PatElem::Rest(sub) => pat_bound_names(sub, out),
                    PatElem::Hole => {}
                }
            }
        }
    }
}

/// Where pattern-bound names are stored: `Declare` creates a new
/// local/global (declaration); `Assign` stores into an existing target.
#[derive(Clone, Copy)]
enum PatStoreMode {
    Declare,
    Assign,
}

/// Whether `s` references `name` (`this` / `arguments`) at THIS function's
/// own level. Nested regular functions/methods have their own `this` and
/// `arguments`, so their bodies are skipped; nested ARROWS inherit this
/// function's bindings, so they are descended into (their hidden captures
/// chain to this function's). Conservative false-positives (an extra hidden
/// capture) are harmless; false-negatives would break semantics.
fn stmt_uses_lexical(s: &Stmt, name: &str) -> bool {
    match s {
        Stmt::Expr(e) => expr_uses_lexical(e, name),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .any(|(_, v)| v.as_ref().map_or(false, |e| expr_uses_lexical(e, name))),
        Stmt::Return(Some(e)) => expr_uses_lexical(e, name),
        Stmt::If { cond, then, els } => {
            expr_uses_lexical(cond, name)
                || stmt_uses_lexical(then, name)
                || els.as_ref().map_or(false, |e| stmt_uses_lexical(e, name))
        }
        Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => {
            expr_uses_lexical(cond, name) || stmt_uses_lexical(body, name)
        }
        Stmt::For { init, cond, update, body } => {
            init.as_ref().map_or(false, |s| stmt_uses_lexical(s, name))
                || cond.as_ref().map_or(false, |e| expr_uses_lexical(e, name))
                || update.as_ref().map_or(false, |e| expr_uses_lexical(e, name))
                || stmt_uses_lexical(body, name)
        }
        Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { obj: iterable, body, .. } => {
            expr_uses_lexical(iterable, name) || stmt_uses_lexical(body, name)
        }
        Stmt::Block(stmts) => stmts.iter().any(|s| stmt_uses_lexical(s, name)),
        Stmt::Labeled { body, .. } => stmt_uses_lexical(body, name),
        Stmt::Throw(e) => expr_uses_lexical(e, name),
        Stmt::Try { body, catch, finally } => {
            stmt_uses_lexical(body, name)
                || catch.as_ref().map_or(false, |(_, b)| stmt_uses_lexical(b, name))
                || finally.as_ref().map_or(false, |b| stmt_uses_lexical(b, name))
        }
        Stmt::Switch { disc, cases } => {
            expr_uses_lexical(disc, name)
                || cases.iter().any(|c| c.body.iter().any(|s| stmt_uses_lexical(s, name)))
        }
        // Own `this`/`arguments`.
        Stmt::FnDecl { .. } | Stmt::Class { .. } => false,
        _ => false,
    }
}

fn expr_uses_lexical(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident(n) => n == name,
        Expr::Bin(_, l, r) => expr_uses_lexical(l, name) || expr_uses_lexical(r, name),
        Expr::Unary(_, x) | Expr::Await(x) | Expr::Delete(x) | Expr::Paren(x) => {
            expr_uses_lexical(x, name)
        }
        Expr::Assign { target, value, .. } => {
            expr_uses_lexical(target, name) || expr_uses_lexical(value, name)
        }
        Expr::Call { callee, args, .. } => {
            expr_uses_lexical(callee, name) || args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::Prop { obj, .. } | Expr::IncDec { target: obj, .. } => expr_uses_lexical(obj, name),
        Expr::Index { obj, index, .. } => expr_uses_lexical(obj, name) || expr_uses_lexical(index, name),
        Expr::Array(elems) => elems.iter().any(|a| expr_uses_lexical(&a.expr, name)),
        Expr::Object(fields) => fields.iter().any(|f| match f {
            ObjElem::Pair(_, v) => expr_uses_lexical(v, name),
            ObjElem::Computed(k, v) => expr_uses_lexical(k, name) || expr_uses_lexical(v, name),
            ObjElem::Spread(s) => expr_uses_lexical(s, name),
        }),
        Expr::Template(parts) => parts.iter().any(|p| match p {
            TemplatePart::Lit(_) => false,
            // Interpolations are raw tokens; a conservative token scan
            // (a false positive only costs an extra hidden capture).
            TemplatePart::Expr(ts) => {
                ts.tokens.iter().any(|t| matches!(t, Token::Ident(n) if n == name))
            }
        }),
        Expr::Ternary { cond, then, els } => {
            expr_uses_lexical(cond, name)
                || expr_uses_lexical(then, name)
                || expr_uses_lexical(els, name)
        }
        Expr::Sequence(es) => es.iter().any(|x| expr_uses_lexical(x, name)),
        Expr::New { callee, args } => {
            expr_uses_lexical(callee, name) || args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::SuperCall { args } | Expr::SuperProp { args: Some(args), .. } => {
            args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::SuperProp { args: None, .. } => false,
        // Nested arrows inherit this function's bindings; regular functions
        // (and class bodies/methods) bind their own.
        Expr::Lambda { body, is_arrow, .. } => *is_arrow && stmt_uses_lexical(body, name),
        Expr::Class { .. } => false,
        _ => false,
    }
}

#[derive(Debug, Clone)]
enum Stmt {
    Expr(Expr),
    /// `let a = 1, { b, c } = obj` — one or more declarators.
    VarDecl { decls: Vec<(Pat, Option<Expr>)> },
    FnDecl { name: String, params: FnParams, body: Box<Stmt>, is_async: bool },
    Return(Option<Expr>),
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Box<Stmt> },
    DoWhile { cond: Expr, body: Box<Stmt> },
    For { init: Option<Box<Stmt>>, cond: Option<Expr>, update: Option<Expr>, body: Box<Stmt> },
    ForOf { pat: Pat, declared: bool, iterable: Expr, body: Box<Stmt> },
    ForIn { pat: Pat, declared: bool, obj: Expr, body: Box<Stmt> },
    Import { src: String, kind: ImportKind },
    Block(Vec<Stmt>),
    Break,
    Continue,
    BreakLabel(String),
    ContinueLabel(String),
    Labeled { name: String, body: Box<Stmt> },
    Throw(Expr),
    Try {
        body: Box<Stmt>,
        catch: Option<(String, Box<Stmt>)>,
        finally: Option<Box<Stmt>>,
    },
    Switch { disc: Expr, cases: Vec<SwitchCase> },
    /// `class Name extends Parent { … }` — a class declaration. The name is
    /// registered like a function declaration; the value is the class.
    Class { name: String, extends: Option<Expr>, methods: Vec<MethodDef> },
    /// `export let a = 1, b = 2` / `export function f() {}` /
    /// `export { a, b }` / `export { a as c }` / `export default expr`.
    /// `pairs` is (public export name, source binding name) — an alias is
    /// `("c", "a")`, a plain declaration `("a", "a")`, and `export default`
    /// `("default", "\0default")`. `stmt` is the declaration to emit (Nop
    /// for the `export { ... }` forms); `default` marks the stored-value form.
    Export { pairs: Vec<(String, String)>, stmt: Box<Stmt>, default: bool },
    Nop,
}

/// What an `import` statement binds, by source kind.
#[derive(Debug, Clone)]
enum ImportKind {
    /// `import { f } from 'alloy:core'` — bind each named builtin.
    Core(Vec<String>),
    /// `import { f } from './x.py' as python` — bind the alias (or first
    /// name) to the Python sidecar module object.
    Python(String),
    /// `import { f, g as h } from './x.ajs'` — (exported name, local binding)
    /// pairs, sugar for `require` + property extraction.
    ModuleNamed(Vec<(String, String)>),
    /// `import * as m from './x.ajs'` — bind the whole exports object.
    ModuleNamespace(String),
    /// `import d from './x.ajs'` — bind the module's default export.
    ModuleDefault(String),
    /// `import './x.ajs'` — run the module for its side effects only.
    ModuleSideEffect,
}

/// One `case value:` / `default:` arm of a switch: the (optional) test and
/// the statements until the next arm. A `test` of `None` is `default`.
#[derive(Debug, Clone)]
struct SwitchCase {
    test: Option<Expr>,
    body: Vec<Stmt>,
}

/// How a function's captured variable is reached at runtime.
#[derive(Clone, Copy)]
enum UpvalueKind {
    /// A local slot in the immediately enclosing function's frame.
    Local { slot: u8 },
    /// An upvalue index in the immediately enclosing function's closure cells.
    Upvalue { index: u8 },
    /// An arrow's hidden lexical capture: the enclosing frame's `this` (or
    /// `arguments`), read via LoadThis/LoadArguments at NewClosure time and
    /// stored in a cell. Arrows never bind their own `this`/`arguments`.
    Lexical,
}

struct UpvalueRef {
    name: String,
    kind: UpvalueKind,
}

struct FuncCtx {
    locals: Vec<String>,
    upvalues: Vec<UpvalueRef>,
    /// Whether this function is `async` (its body may contain `await`).
    is_async: bool,
    /// Set when the body references `arguments`: the VM then snapshots the
    /// passed args into the call frame at entry (the frame's local slots
    /// overwrite the arg region as the body runs, so a lazy read would see
    /// locals, not args). Serialized in the NewClosure operand; functions
    /// that never touch `arguments` pay nothing.
    uses_arguments: bool,
    /// Whether this function is an arrow (`x => ...`). Arrows bind `this`
    /// and `arguments` lexically: their bodies reference hidden `\0this` /
    /// `\0arguments` upvalues captured at creation, never LoadThis/
    /// LoadArguments of their own frame.
    is_arrow: bool,
}

struct LoopCtx {
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    continue_target: usize,
    /// `trys.len()` when this loop was pushed. `break`/`continue` run the
    /// finallys of trys nested *inside* the loop only — trys enclosing the
    /// loop are not exited, so their finallys must not run at the exit site.
    trys_depth: usize,
    /// Name of a label directly attached to this loop (`outer: for ...`), if
    /// any; its `continue` jumps are patched together with this loop's.
    label: Option<String>,
    /// Switch contexts are `break` targets but not `continue` targets; an
    /// unlabeled `continue` skips them and targets the enclosing loop.
    is_switch: bool,
}

/// Where a `break`/`continue` jump is recorded: an index into `loops` (the
/// innermost break/continue target), or into `labels`.
enum ExitTarget {
    Loop(usize),
    Label(usize),
}

/// A labeled statement being compiled: `name: stmt`. `break name` jumps past
/// it; `continue name` is only legal when the label is on a loop.
struct LabelCtx {
    name: String,
    is_loop: bool,
    /// `trys.len()` when the label was pushed — a labeled exit runs the
    /// finallys of trys between the exit site and the labeled statement.
    trys_depth: usize,
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    continue_target: usize,
}

/// An active `try` while compiling its body. `finally` bodies are re-emitted
/// inline at every `break`/`continue`/`return` exit site (JS semantics).
struct TryCtx {
    finally: Option<Box<Stmt>>,
}

enum Resolved {
    Local(u8),
    Upvalue(u8),
    Global(u16),
}

/// A token that can START an expression. When one of these directly follows a
/// complete expression (no operator, no postfix), the source is invalid JS —
/// `0.toString` (the lexer reads `0.` as a number, then the identifier)
/// `print(1 2)`, `a b`, `x = 5 let y = 2`. Previously the parser silently
/// swallowed these: the dangling token became a second call argument, array
/// element, or statement. `LBrace` is deliberately excluded so `a {}` remains
/// two statements (JS permits it; the call/array/object lists reject `f(a {})`
/// via their comma checks), and `LParen`/`Dot`/`LBracket`/`PlusPlus`/
/// `MinusMinus`/`Arrow` never reach here (the postfix loop consumes them).
/// Map a keyword token back to its source text. Keywords are legal property
/// names in JS (the IdentifierName rule): `m.default`, `o.delete`, `{ if: 1 }`
/// all parse even though the words are reserved. Returns None for non-keyword
/// tokens (operators, literals) that cannot name a property.
fn keyword_text(t: &Token) -> Option<String> {
    Some(match t {
        Token::Ident(s) => return Some(s.clone()),
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
        _ => return None,
    })
}

fn starts_expression(t: &Token) -> bool {
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
    )
}

/// Reserved global holding a module's `export default` value. Collision-proof:
/// `\0` cannot appear in source identifiers, so no user binding shadows it.
pub(crate) const DEFAULT_EXPORT: &str = "\0default";

/// All identifier names bound by a destructuring pattern (`let { a, b } = o`
/// exports both `a` and `b`; `let [x, ...rest] = a` exports `x` and `rest`).
fn pat_names(p: &Pat) -> Vec<String> {
    match p {
        Pat::Bind(n) => vec![n.clone()],
        Pat::Object(fields) => fields.iter().flat_map(|e| match e {
            ObjPatElem::Key(_, p) | ObjPatElem::Computed(_, p) | ObjPatElem::Rest(p) => pat_names(p),
        }).collect(),
        Pat::Array(elems) => elems
            .iter()
            .flat_map(|e| match e {
                PatElem::Bind(p) | PatElem::Rest(p) => pat_names(p),
                PatElem::Hole => vec![],
            })
            .collect(),
    }
}

fn is_native(name: &str) -> bool {
    matches!(
        name,
        "print" | "http" | "memory" | "fs" | "Promise" | "setTimeout" | "setInterval"
            | "clearTimeout" | "clearInterval" | "queueMicrotask" | "console" | "channel"
            | "spawn" | "Date" | "Math" | "JSON" | "Number" | "Object" | "Array" | "String"
            | "parseInt" | "parseFloat" | "isNaN" | "require" | "reload" | "sweepSegments"
            | "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError"
            | "EvalError" | "URIError" | "NaN" | "Infinity"
    )
}

/// The opcode implementing a compound-assignment operator, if `op` is one.
fn compound_opcode(op: &str) -> Option<Opcode> {
    match op {
        "+=" => Some(Opcode::Add),
        "-=" => Some(Opcode::Subtract),
        "*=" => Some(Opcode::Multiply),
        "/=" => Some(Opcode::Divide),
        "%=" => Some(Opcode::Modulo),
        "&=" => Some(Opcode::BitAnd),
        "|=" => Some(Opcode::BitOr),
        "^=" => Some(Opcode::BitXor),
        "<<=" => Some(Opcode::Shl),
        ">>=" => Some(Opcode::Shr),
        ">>>=" => Some(Opcode::UShr),
        "**=" => Some(Opcode::Pow),
        _ => None,
    }
}

/// Arithmetic codes for the fused superinstructions
/// (0=Add 1=Subtract 2=Multiply 3=Divide 4=Modulo 5=BitAnd 6=BitOr 7=BitXor
/// 8=Shl 9=Shr 10=UShr).
fn arith_code(op: &str) -> Option<u8> {
    match op {
        "+" => Some(0),
        "-" => Some(1),
        "*" => Some(2),
        "/" => Some(3),
        "%" => Some(4),
        "&" => Some(5),
        "|" => Some(6),
        "^" => Some(7),
        "<<" => Some(8),
        ">>" => Some(9),
        ">>>" => Some(10),
        "**" => Some(11),
        _ => None,
    }
}    /// A step in a fused ArithChain: header byte (`kind << 5 | ar`) + operand.
    /// Kinds: 0=LoadLocal (operand = slot), 1=Const (operand = i32 imm),
    /// 2=Save (push acc onto the operand stack), 3=Combine (pop t; acc = t ar
    /// acc). Chain ar codes are `arith_code + 1` (0 = init): a LoadLocal/Const
    /// step with ar=0 initializes acc, every other step applies
    /// `acc = acc ar operand`.
#[derive(Clone, Copy)]
struct ChainStep {
    hdr: u8,
    op: i64,
}

impl ChainStep {
    // Header = kind << 5 | ar (ar bits 0-4, kind bits 5-7). The kind
    // constants below are already shifted into the header position.
    const LOAD: u8 = 0;
    const CONST: u8 = 1 << 5;
    const SAVE: u8 = 2 << 5;
    const COMBINE: u8 = 3 << 5;
}

impl Compiler {
    /// A leaf usable as an ArithChain operand: a local or an int literal
    /// (paren-wrapped ok). Everything else (calls, indexes, globals,
    /// upvalues, strings…) is not chainable — those fall back to the normal
    /// stack emission, so the fused opcode is never asked to handle a value
    /// whose evaluation has side effects or unknown identity.
    fn chain_leaf(&mut self, e: &Expr) -> Option<ChainStep> {
        match e {
            Expr::Paren(inner) => self.chain_leaf(inner),
            Expr::Int(v) => Some(ChainStep { hdr: ChainStep::CONST, op: *v }),
            Expr::Ident(name) => match self.resolve(name) {
                Resolved::Local(s) => Some(ChainStep { hdr: ChainStep::LOAD, op: s as i64 }),
                _ => None,
            },
            _ => None,
        }
    }

    /// Linearize an int-arithmetic tree (add/sub/mul/div/mod/bitwise/pow over
    /// locals and literals) into ArithChain steps. Left-associative spines
    /// with leaf right-operands become straight `acc = acc ar leaf` steps;
    /// a right operand that is itself a subtree becomes Save + subtree +
    /// Combine (`acc = t ar acc`), which nests to any depth via the operand
    /// stack. Returns the number of applied arithmetic ops, or None if `e` is
    /// not chainable. The compiled chain is exactly equivalent to running the
    /// same sequence of ADD/SUB/… opcodes (per-step i64 fast path with the
    /// generic Value fallback), so evaluation order and coercion semantics
    /// are preserved by construction.
    fn build_chain(&mut self, e: &Expr, steps: &mut Vec<ChainStep>) -> Option<usize> {
        match e {
            Expr::Paren(inner) => self.build_chain(inner, steps),
            Expr::Bin(op, l, r) => self.build_chain_node(op, l, r, steps),
            _ => None,
        }
    }

    /// Build steps for one binary node `l op r` (used at the chain root and
    /// recursively by [`build_chain`]). See [`build_chain`] for the
    /// Save/Combine nesting rules.
    fn build_chain_node(
        &mut self,
        op: &str,
        l: &Expr,
        r: &Expr,
        steps: &mut Vec<ChainStep>,
    ) -> Option<usize> {
        let ar = arith_code(op)?;
        let left_ops = match self.chain_leaf(l) {
            Some(s) => {
                steps.push(s);
                0
            }
            None => self.build_chain(l, steps)?,
        };
        // Chain ar codes are ar_code + 1 (0 = init marker): the compiler's
        // `+` maps to 0 in arith_code, which must not collide with the init
        // step's ar=0.
        let right_ops = match self.chain_leaf(r) {
            Some(mut s) => {
                s.hdr |= ar + 1;
                steps.push(s);
                1
            }
            None => {
                steps.push(ChainStep { hdr: ChainStep::SAVE, op: 0 });
                let sub = self.build_chain(r, steps)?;
                steps.push(ChainStep { hdr: ChainStep::COMBINE | (ar + 1), op: 0 });
                sub + 1
            }
        };
        Some(left_ops + right_ops)
    }

    fn emit_chain(&mut self, steps: &[ChainStep], term: u8) {
        self.program.emit_op(Opcode::ArithChain);
        self.program.emit_u8(steps.len() as u8);
        self.program.emit_u8(term);
        for s in steps {
            self.program.emit_u8(s.hdr);
            self.program.emit_i32(s.op as i32);
        }
    }

    /// Emit `l op r` as an ArithChain with the given terminal (`0` discard,
    /// `0x80` push, `0x40|slot` store, `0xC0|slot` store+push). Returns true
    /// when the chain fired: ≥ 2 ops (single ops are already fused to
    /// BinLocalInt/BinLocalLocal at 1 dispatch, so a chain would only add
    /// decode) and at least one local operand (a pure-constant tree is left
    /// to the peephole's constant fold, which precomputes it to a single
    /// LoadConst).
    fn emit_arith_chain(&mut self, op: &str, l: &Expr, r: &Expr, term: u8) -> bool {
        // Dev A/B switch: ALLOY_NO_CHAIN=1 disables the register-ALU fusion
        // (compile-time only; zero runtime cost) for measuring its effect.
        if std::env::var("ALLOY_NO_CHAIN").is_ok() {
            return false;
        }
        let mut steps = Vec::new();
        let Some(ops) = self.build_chain_node(op, l, r, &mut steps) else {
            return false;
        };
        let has_local = steps.iter().any(|s| s.hdr & 0xE0 == ChainStep::LOAD);
        if ops < 2 || !has_local || steps.len() > 48 {
            return false;
        }
        self.emit_chain(&steps, term);
        true
    }

    /// Emit `x = <chain>` / `x op= <chain>` (local target) as one ArithChain
    /// with a store terminal. Always fires when the RHS is chainable — even a
    /// single-op RHS beats the current LoadLocal + value + ArithStoreLocal /
    /// fused-op + StoreLocal sequences. Falls back (false) for slots ≥ 64
    /// (the store terminal holds 6 bits) — the existing path still chains the
    /// value via emit_arith_chain.
    fn try_emit_chain_assign(&mut self, s: u8, op: &str, value: &Expr, keep: bool) -> bool {
        // Dev A/B switch (see emit_arith_chain).
        if std::env::var("ALLOY_NO_CHAIN").is_ok() {
            return false;
        }
        if s >= 64 {
            return false;
        }
        let mut steps = Vec::new();
        if op == "=" {
            if self.build_chain(value, &mut steps).is_none() || steps.len() > 48 {
                return false;
            }
        } else {
            // x op= v  =>  x = x op v. A leaf RHS applies directly
            // (`acc = x op leaf`, 3 steps); a subtree needs the save/combine
            // bracket (`acc = x op [subtree]`).
            let Some(ar) = arith_code(op.trim_end_matches('=')) else {
                return false;
            };
            steps.push(ChainStep { hdr: ChainStep::LOAD, op: s as i64 });
            match self.chain_leaf(value) {
                Some(mut st) => {
                    st.hdr |= ar + 1;
                    steps.push(st);
                }
                None => {
                    steps.push(ChainStep { hdr: ChainStep::SAVE, op: 0 });
                    if self.build_chain(value, &mut steps).is_none() {
                        return false;
                    }
                    steps.push(ChainStep { hdr: ChainStep::COMBINE | (ar + 1), op: 0 });
                }
            }
            if steps.len() > 48 {
                return false;
            }
        }
        // Emit the chain as one of the fixed-shape register-ALU
        // superinstructions (lean straight-line handlers — the variable
        // ArithChain's per-step decode costs more than the dispatches it
        // replaces on these shapes). Non-matching chains fall back to the
        // normal emission below. The ar bytes are the raw arith_code with
        // bit 7 = keep; each step's enc_ar (arith_code + 1) is decoded back.
        let keepb = if keep { 0x80 } else { 0 };
        // [LOAD s][CONST imm ar]  ->  Arith2StoreLocalConst
        if steps.len() == 2
            && steps[0].hdr & 0xE0 == ChainStep::LOAD
            && steps[0].op as u8 == s
            && steps[1].hdr & 0xE0 == ChainStep::CONST
        {
            let ar = (steps[1].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith2StoreLocalConst);
            self.program.emit_u8(s);
            self.program.emit_u8(ar | keepb);
            self.program.emit_i32(steps[1].op as i32);
            return true;
        }
        // [LOAD s][CONST i1 ar1][CONST i2 ar2]  ->  Arith3StoreLocalConstConst
        if steps.len() == 3
            && steps[0].hdr & 0xE0 == ChainStep::LOAD
            && steps[0].op as u8 == s
            && steps[1].hdr & 0xE0 == ChainStep::CONST
            && steps[2].hdr & 0xE0 == ChainStep::CONST
        {
            let ar1 = (steps[1].hdr & 0x1F) - 1;
            let ar2 = (steps[2].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith3StoreLocalConstConst);
            self.program.emit_u8(s);
            self.program.emit_u8(ar1);
            self.program.emit_i32(steps[1].op as i32);
            self.program.emit_u8(ar2 | keepb);
            self.program.emit_i32(steps[2].op as i32);
            return true;
        }
        // [CONST c1][LOAD s ar1][CONST c2 ar2]  ->  Arith3StoreConstLocalConst
        if steps.len() == 3
            && steps[0].hdr & 0xE0 == ChainStep::CONST
            && steps[1].hdr & 0xE0 == ChainStep::LOAD
            && steps[1].op as u8 == s
            && steps[2].hdr & 0xE0 == ChainStep::CONST
        {
            let ar1 = (steps[1].hdr & 0x1F) - 1;
            let ar2 = (steps[2].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith3StoreConstLocalConst);
            self.program.emit_i32(steps[0].op as i32);
            self.program.emit_u8(ar1);
            self.program.emit_u8(s);
            self.program.emit_u8(ar2 | keepb);
            self.program.emit_i32(steps[2].op as i32);
            return true;
        }
        false
    }
}

/// Comparison codes for the fused compare superinstructions, mirroring the
/// VM's Less/Greater/LessEqual/GreaterEqual/Equal/NotEqual/StrictEqual/
/// StrictNotEqual opcodes (7 = `!==`, the strict negation of `===`).
fn cmp_code(op: &str) -> Option<u8> {
    match op {
        "<" => Some(0),
        "<=" => Some(1),
        ">" => Some(2),
        ">=" => Some(3),
        "==" => Some(4),
        "!=" => Some(5),
        "===" => Some(6),
        "!==" => Some(7),
        _ => None,
    }
}

/// Arith code for a compound-assignment operator, if `op` is one.
fn compound_arith(op: &str) -> Option<u8> {
    match op {
        "+=" => Some(0),
        "-=" => Some(1),
        "*=" => Some(2),
        "/=" => Some(3),
        "%=" => Some(4),
        "&=" => Some(5),
        "|=" => Some(6),
        "^=" => Some(7),
        "<<=" => Some(8),
        ">>=" => Some(9),
        ">>>=" => Some(10),
        "**=" => Some(11),
        _ => None,
    }
}

pub struct Compiler {
    program: Program,
    /// Stack of function contexts; index 0 is the innermost (current) function.
    funcs: Vec<FuncCtx>,
    /// Break/continue targets for enclosing loops.
    loops: Vec<LoopCtx>,
    /// When true, top-level declarations are stored as globals (REPL mode).
    top_globals: bool,
    /// When true, `export` declarations are legal and recorded into
    /// `program.exports` (module mode, set by `compile_module`). In ordinary
    /// script mode `export` is a loud compile error — Node also rejects it
    /// outside ES modules.
    in_module: bool,
    /// Names declared at the top level (globals), as opposed to merely read:
    /// `delete x` on a declared binding is false, on an undeclared global is
    /// true (sloppy JS), and the globals table itself conflates the two.
    declared_globals: std::collections::HashSet<String>,
    /// Names bound by `import` statements (module files). Assigning to an
    /// import is an ESM SyntaxError — here it's a loud compile error, so a
    /// live-import cell can never be clobbered from the importing scope.
    imported_bindings: std::collections::HashSet<String>,
    /// Counter for compiler-generated temporary names (for-of/in desugaring).
    fresh_counter: u32,
    /// Enclosing `try` bodies being compiled, innermost last. Exiting one via
    /// `break`/`continue`/`return` runs its `finally` inline first.
    trys: Vec<TryCtx>,
    /// Slots pre-allocated for variables declared inside `finally` bodies, so
    /// re-emitting the body at exit sites stores into the same slot instead of
    /// allocating a new one each time.
    finally_locals: std::collections::HashMap<String, u8>,
    /// Enclosing labeled statements, innermost last.
    labels: Vec<LabelCtx>,
    /// Set right before emitting a loop that is directly labeled; the loop
    /// emitter consumes it so `continue label` targets the right loop even
    /// when the label stack's top belongs to an enclosing labeled statement.
    pending_label: Option<String>,
    /// Whether the expression currently being emitted must leave its value on
    /// the stack. False only for a top-level statement expression that is an
    /// assignment or inc/dec (their value is discarded): those arms then emit
    /// the fused ops with keep=0 and leave nothing, so the statement emits no
    /// trailing Pop. Sub-expressions are always emitted with this true (their
    /// values are consumed by their parent), which the keep-aware arms enforce
    /// by scoping their operand emissions.
    keep_result: bool,
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            program: Program::new(),
            funcs: vec![FuncCtx {
                locals: Vec::new(),
                upvalues: Vec::new(),
                is_async: false,
                uses_arguments: false,
                is_arrow: false,
            }],
            loops: Vec::new(),
            top_globals: false,
            in_module: false,
            imported_bindings: std::collections::HashSet::new(),
            declared_globals: std::collections::HashSet::new(),
            fresh_counter: 0,
            trys: Vec::new(),
            finally_locals: std::collections::HashMap::new(),
            labels: Vec::new(),
            pending_label: None,
            keep_result: true,
        }
    }

    /// Emit `e` with its value discarded: the top-level assignment/inc/dec
    /// leaves no result (fused ops get keep=0) and the caller must not emit a
    /// trailing Pop. Restores `keep_result` afterwards.
    fn emit_discard(&mut self, e: &Expr) -> Result<(), CompileError> {
        let saved = self.keep_result;
        self.keep_result = false;
        let r = self.emit_expr(e);
        self.keep_result = saved;
        r
    }

    /// Emit an expression in statement (value-discarded) position. Assignments,
    /// inc/dec, and calls are keep-aware: they leave nothing, so no Pop.
    /// Everything else still pushes its value, so the trailing Pop stays.
    fn emit_stmt_expr(&mut self, e: &Expr) -> Result<(), CompileError> {
        // Optional chains always push their value (their short-circuit paths
        // produce undefined at the same stack position), so they cannot use
        // the keep=0 call variants — the statement discards with a Pop.
        if Self::is_optional_chain(e) {
            self.emit_expr(e)?;
            self.program.emit_op(Opcode::Pop);
            return Ok(());
        }
        match e {
            Expr::Assign { .. } | Expr::IncDec { .. } | Expr::Call { .. } | Expr::Sequence(..) => {
                self.emit_discard(e)
            }
            _ => {
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Pop);
                Ok(())
            }
        }
    }

    /// Emit `delete target` per JS semantics: a property/index expression
    /// deletes the member (evaluating the reference first, exactly once); a
    /// plain identifier cannot be deleted (false); any other expression
    /// evaluates for its side effects and yields true.
    fn emit_delete(&mut self, target: &Expr) -> Result<(), CompileError> {
        // `delete a?.b` is a SyntaxError in JS.
        if Self::is_optional_chain(target) {
            return Err(CompileError::UnexpectedToken(
                "invalid delete target: optional chaining cannot be deleted".to_string(),
            ));
        }
        match target {
            Expr::Paren(inner) => self.emit_delete(inner),
            Expr::Prop { obj, prop, .. } => {
                self.emit_keep(|s| s.emit_expr(obj))?;
                let ci = self.program.add_constant(Value::string(prop.clone()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(ci);
                self.program.emit_op(Opcode::DeleteProp);
                Ok(())
            }
            Expr::Index { obj, index, .. } => {
                self.emit_keep(|s| s.emit_expr(obj))?;
                self.emit_keep(|s| s.emit_expr(index))?;
                self.program.emit_op(Opcode::DeleteIndex);
                Ok(())
            }
            Expr::Ident(name) => {
                // `delete x`: a bound identifier (local, upvalue, declared
                // top-level global, or native) can't be deleted — false. An
                // undeclared global reference deletes to true (sloppy JS).
                let bound = self.funcs.last().unwrap().locals.iter().any(|l| l == name)
                    || self.funcs.iter().skip(1).any(|f| f.locals.iter().any(|l| l == name))
                    || self.declared_globals.contains(name)
                    || is_native(name);
                self.program.emit_op(if bound { Opcode::LoadFalse } else { Opcode::LoadTrue });
                Ok(())
            }
            other => {
                // `delete f()`, `delete (5)`, `delete (a ? b : c)` … —
                // evaluate for side effects, the result is true.
                self.emit_stmt_expr(other)?;
                self.program.emit_op(Opcode::LoadTrue);
                Ok(())
            }
        }
    }

    /// Run `emit` with the surrounding keep context forced to true (the value
    /// of a sub-expression is always consumed by its parent).
    fn emit_keep(&mut self, emit: impl FnOnce(&mut Self) -> Result<(), CompileError>) -> Result<(), CompileError> {
        let saved = self.keep_result;
        self.keep_result = true;
        let r = emit(self);
        self.keep_result = saved;
        r
    }

    /// Emit `require('<src>')` as a call that KEEPS its result (the module
    /// object), for `import` statements compiled as require sugar. The
    /// require global is resolved like any identifier; LoadGlobal of a
    /// builtin seeds the native.
    fn emit_require_call(&mut self, src: &str) -> Result<(), CompileError> {
        let call = Expr::Call {
            callee: Box::new(Expr::Ident("require".to_string())),
            args: vec![Elem {
                spread: false,
                hole: false,
                expr: Expr::Str(src.to_string()),
            }],
            optional: false,
        };
        self.emit_keep(|c| c.emit_expr(&call))
    }

    /// Emit a discarded condition (`&&` / `||` or a plain expression) as
    /// jump-with-pop edges instead of materializing a boolean value, for
    /// contexts that test the condition and never need its value: loop
    /// conditions, if/ternary tests. The condition is decomposed so each
    /// operand is evaluated exactly once, in JS order, with untaken operands
    /// never evaluated and every operand's value popped by the edge that
    /// tests it:
    ///
    /// - `a && b`:   `a; JumpIfFalsePop exit; b; JumpIfFalsePop exit`
    /// - `a || b`:   `a; JumpIfTruePop body; b; JumpIfFalsePop exit`
    /// - plain `e`:  `e; JumpIfFalsePop exit`
    ///
    /// `exit` collects the falsy-edge jumps (patched to the loop exit / else
    /// path after the body) and `body` the truthy-edge jumps (patched to the
    /// body start). Fall-through means "continue evaluating the next piece of
    /// the condition"; the innermost fall-through is the body itself.
    ///
    /// Compounds nest correctly on either side: `(a || b) && c` emits
    /// `a; JumpIfTruePop [skip b]; b; JumpIfFalsePop exit; c; JumpIfFalsePop
    /// exit` (a truthy `a` still tests `c`), and `a && b || c` emits
    /// `a; JumpIfFalsePop [skip b]; b; JumpIfTruePop body; c; JumpIfFalsePop
    /// exit`.
    fn emit_cond(
        &mut self,
        e: &Expr,
        exit: &mut Vec<usize>,
        body: &mut Vec<usize>,
    ) -> Result<(), CompileError> {
        match e {
            Expr::Paren(inner) => self.emit_cond(inner, exit, body)?,
            Expr::Bin(op, l, r) if *op == "&&" => {
                self.emit_cond_false(l, exit)?;
                self.emit_cond(r, exit, body)?;
            }
            Expr::Bin(op, l, r) if *op == "||" => {
                self.emit_cond_true(l, body)?;
                self.emit_cond(r, exit, body)?;
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                exit.push(self.emit_jump(Opcode::JumpIfFalsePop));
            }
        }
        Ok(())
    }

    /// Emit `e` so a falsy result jumps to `exit` and a truthy result falls
    /// through (continuing with the next piece of the surrounding condition).
    fn emit_cond_false(&mut self, e: &Expr, exit: &mut Vec<usize>) -> Result<(), CompileError> {
        match e {
            Expr::Bin(op, l, r) if *op == "&&" => {
                self.emit_cond_false(l, exit)?;
                self.emit_cond_false(r, exit)?;
            }
            Expr::Bin(op, l, r) if *op == "||" => {
                // A truthy `l` makes the whole `||` truthy, so it must skip
                // past `r` (a forward jump, patched after `r` is emitted).
                let mut skip = Vec::new();
                self.emit_cond_true(l, &mut skip)?;
                self.emit_cond_false(r, exit)?;
                let after = self.program.bytecode.len();
                for j in skip { self.patch_jump_to(j, after); }
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                exit.push(self.emit_jump(Opcode::JumpIfFalsePop));
            }
        }
        Ok(())
    }

    /// Emit `e` so a truthy result jumps to `body` and a falsy result falls
    /// through (continuing with the next piece of the surrounding condition).
    fn emit_cond_true(&mut self, e: &Expr, body: &mut Vec<usize>) -> Result<(), CompileError> {
        match e {
            Expr::Bin(op, l, r) if *op == "||" => {
                self.emit_cond_true(l, body)?;
                self.emit_cond_true(r, body)?;
            }
            Expr::Bin(op, l, r) if *op == "&&" => {
                // A falsy `l` makes the whole `&&` falsy, so it must skip
                // past `r` (a forward jump, patched after `r` is emitted).
                let mut skip = Vec::new();
                self.emit_cond_false(l, &mut skip)?;
                self.emit_cond_true(r, body)?;
                let after = self.program.bytecode.len();
                for j in skip { self.patch_jump_to(j, after); }
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                body.push(self.emit_jump(Opcode::JumpIfTruePop));
            }
        }
        Ok(())
    }

    /// Collect variable names declared directly within `stmt`'s body (not
    /// descending into nested functions, which allocate their own locals).
    fn collect_declared(stmt: &Stmt, out: &mut Vec<String>) {
        match stmt {
            Stmt::VarDecl { decls } => {
                for (pat, _) in decls {
                    Self::collect_pat_names(pat, out);
                }
            }
            Stmt::Block(ss) => {
                for s in ss {
                    Self::collect_declared(s, out);
                }
            }
            Stmt::If { then, els, .. } => {
                Self::collect_declared(then, out);
                if let Some(e) = els {
                    Self::collect_declared(e, out);
                }
            }
            Stmt::While { body, .. } => Self::collect_declared(body, out),
            Stmt::For { init, body, .. } => {
                if let Some(i) = init {
                    Self::collect_declared(i, out);
                }
                Self::collect_declared(body, out);
            }
            Stmt::ForOf { body, .. } | Stmt::ForIn { body, .. } => {
                Self::collect_declared(body, out)
            }
            Stmt::Try { body, catch, finally, .. } => {
                Self::collect_declared(body, out);
                if let Some((_, c)) = catch {
                    Self::collect_declared(c, out);
                }
                if let Some(f) = finally {
                    Self::collect_declared(f, out);
                }
            }
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    for s in &c.body {
                        Self::collect_declared(s, out);
                    }
                }
            }
            // FnDecl / Lambda bodies and imports declare nothing here.
            _ => {}
        }
    }

    /// Collect the bound names of a destructuring pattern.
    fn collect_pat_names(pat: &Pat, out: &mut Vec<String>) {
        match pat {
            Pat::Bind(n) => out.push(n.clone()),
            Pat::Object(fields) => {
                for f in fields {
                    match f {
                        ObjPatElem::Key(_, sub) | ObjPatElem::Computed(_, sub) | ObjPatElem::Rest(sub) => {
                            Self::collect_pat_names(sub, out);
                        }
                    }
                }
            }
            Pat::Array(elems) => {
                for el in elems {
                    match el {
                        PatElem::Bind(sub) | PatElem::Rest(sub) => {
                            Self::collect_pat_names(sub, out);
                        }
                        PatElem::Hole => {}
                    }
                }
            }
        }
    }

    /// Pre-allocate slots for a `finally` body's declared variables (once), so
    /// every re-emission of the body writes the same slot. At the REPL top
    /// level declarations are globals (name-keyed), so no allocation is needed.
    fn ensure_finally_locals(&mut self, f: &Stmt) {
        if self.top_globals && self.funcs.len() == 1 {
            return;
        }
        let mut names = Vec::new();
        Self::collect_declared(f, &mut names);
        for name in names {
            if !self.finally_locals.contains_key(&name) {
                let s = self.funcs.last_mut().unwrap().locals.len() as u8;
                self.funcs.last_mut().unwrap().locals.push(name.clone());
                self.finally_locals.insert(name, s);
            }
        }
    }

    /// Emit a `finally` body (re-entrant: used for the normal path, the
    /// handler path, and every exit site).
    fn emit_finally(&mut self, f: &Stmt) -> Result<(), CompileError> {
        self.ensure_finally_locals(f);
        self.emit_stmt(f)
    }

    fn fresh_name(&mut self, base: &str) -> String {
        self.fresh_counter += 1;
        format!("${}{}", base, self.fresh_counter)
    }

    /// Allocate a hidden compiler-generated local slot (for temporaries such
    /// as the object/index of an increment target).
    fn fresh_local(&mut self) -> u8 {
        let s = self.funcs.last_mut().unwrap().locals.len() as u8;
        let name = self.fresh_name("t");
        self.funcs.last_mut().unwrap().locals.push(name);
        s
    }

    fn resolve_global(&mut self, name: &str) -> u16 {
        if let Some(i) = self.program.globals.iter().position(|g| g == name) {
            i as u16
        } else {
            let i = self.program.globals.len() as u16;
            self.program.globals.push(name.to_string());
            i
        }
    }

    fn global_store_index(&mut self, name: &str) -> Result<u16, CompileError> {
        if is_native(name) {
            return Err(CompileError::CannotShadowBuiltin(name.to_string()));
        }
        Ok(self.resolve_global(name))
    }

    /// Resolve a variable to a local slot, an upvalue index, or a global slot.
    fn resolve(&mut self, name: &str) -> Resolved {
        if let Some(s) = self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
            return Resolved::Local(s as u8);
        }
        if let Some(u) = self.resolve_upvalue(name) {
            return Resolved::Upvalue(u);
        }
        Resolved::Global(self.resolve_global(name))
    }

    /// Find `name` in an enclosing function's locals and build (or reuse) the
    /// upvalue chain from the current function up to the owner. Returns the
    /// upvalue index in the current function's context.
    ///
    /// `funcs` is a stack with the outermost (main) context at index 0 and the
    /// innermost (current) function at the last index.
    fn resolve_upvalue(&mut self, name: &str) -> Option<u8> {
        let n = self.funcs.len();
        for owner in (0..n - 1).rev() {
            if let Some(slot) = self.funcs[owner].locals.iter().rposition(|l| l == name) {
                let mut target_idx = slot as u8;
                let mut target_is_local = true;
                for level in (owner + 1)..n {
                    let entry = if target_is_local {
                        UpvalueRef { name: name.to_string(), kind: UpvalueKind::Local { slot: target_idx } }
                    } else {
                        UpvalueRef { name: name.to_string(), kind: UpvalueKind::Upvalue { index: target_idx } }
                    };
                    let idx = match self.funcs[level].upvalues.iter().position(|u| u.name == name) {
                        Some(i) => i as u8,
                        None => {
                            self.funcs[level].upvalues.push(entry);
                            (self.funcs[level].upvalues.len() - 1) as u8
                        }
                    };
                    if level == n - 1 {
                        return Some(idx);
                    }
                    target_idx = idx;
                    target_is_local = false;
                }
            }
        }
        None
    }

    /// Where an arrow's hidden `\0this`/`\0arguments` capture comes from.
    /// Scanning the enclosing functions innermost-first: the first arrow
    /// that already holds the hidden capture provides it (a nested arrow
    /// re-captures that cell, so a chain of arrows shares one value no
    /// matter how the inner arrows are later called); the first non-arrow
    /// function binds the value itself, so the capture reads it from that
    /// frame at creation time (Lexical). With no enclosing function at all
    /// (top level), the capture is still Lexical: LoadThis/LoadArguments
    /// yield undefined, matching a top-level ESM binding.
    fn lexical_capture_kind(&self, hidden: &str) -> UpvalueKind {
        for f in self.funcs.iter().rev() {
            if f.is_arrow {
                if let Some(i) = f.upvalues.iter().position(|u| u.name == hidden) {
                    return UpvalueKind::Upvalue { index: i as u8 };
                }
                // An arrow without this hidden capture does not bind the
                // name, so it passes through lexically — keep scanning.
            } else {
                return UpvalueKind::Lexical;
            }
        }
        UpvalueKind::Lexical
    }

    /// Index of the current function's hidden `\0this`/`\0arguments` capture
    /// (added to the FuncCtx when the arrow was pushed). `None` only when the
    /// body never referenced the name, which the emission path guarantees
    /// cannot be reached for a reference that exists.
    fn arrow_hidden_index(&self, hidden: &str) -> Option<u8> {
        self.funcs
            .last()
            .and_then(|f| f.upvalues.iter().position(|u| u.name == hidden))
            .map(|i| i as u8)
    }

    /// `target &&= / ||= / ??= value` — short-circuit assignment. The target's
    /// reference (receiver, optional index) is evaluated exactly once and
    /// stashed in temps; the current value is read and tested, and only when
    /// the test fails (falsy / truthy / nullish) is the RHS evaluated and
    /// written. The expression's value is the old value on the short-circuit
    /// path, the new value otherwise — matching JS.
    fn emit_logical_assign(
        &mut self,
        op: &str,
        target: &Expr,
        value: &Expr,
        keep: bool,
    ) -> Result<(), CompileError> {
        let (test, nullish) = match op {
            "&&=" => (Opcode::JumpIfFalse, false),
            "||=" => (Opcode::JumpIfTrue, false),
            _ => (Opcode::JumpIfNullish, true),
        };
        let store = match target {
            Expr::Ident(name) => match self.resolve(name) {
                Resolved::Local(s) => LStore::Local(s),
                Resolved::Upvalue(i) => LStore::Upvalue(i),
                Resolved::Global(i) => LStore::Global(i),
            },
            Expr::Prop { obj, prop, .. } => {
                let t = self.fresh_local();
                self.emit_keep(|c| c.emit_expr(obj))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(t);
                let pi = self.program.add_constant(Value::string(prop.clone()));
                LStore::Prop(pi, t)
            }
            Expr::Index { obj, index, .. } => {
                let to = self.fresh_local();
                let ti = self.fresh_local();
                self.emit_keep(|c| c.emit_expr(obj))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(to);
                self.emit_keep(|c| c.emit_expr(index))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(ti);
                LStore::Index(to, ti)
            }
            // Invalid targets (calls, literals, …) fall through to the main
            // assignment arm, which reports the error.
            _ => return Ok(()),
        };
        // Read the current value (through the stash for members).
        match &store {
            LStore::Local(s) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*s);
            }
            LStore::Upvalue(i) => {
                self.program.emit_op(Opcode::LoadUpvalue);
                self.program.emit_u8(*i);
            }
            LStore::Global(i) => {
                self.program.emit_op(Opcode::LoadGlobal);
                self.program.emit_u16(*i);
            }
            // GetProperty reads its key from the inline constant — it does
            // NOT consume a key from the stack (only the object), so no
            // LoadConst here; that would leave a stray key on the stack.
            LStore::Prop(pi, t) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*t);
                self.program.emit_op(Opcode::GetProperty);
                self.program.emit_u16(*pi);
            }
            LStore::Index(to, ti) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*to);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*ti);
                self.program.emit_op(Opcode::GetIndex);
            }
        }
        self.program.emit_op(Opcode::Dup);
        let j = self.emit_jump(test);
        if nullish {
            // JumpIfNullish consumes the dup, so the non-nullish path (which
            // already holds the old value as the result) must jump over the
            // write; the nullish path replaces the old value.
            let end = self.emit_jump(Opcode::Jump);
            let pop_at = self.program.bytecode.len();
            self.program.emit_op(Opcode::Pop);
            self.emit_value_store(&store, value, keep)?;
            self.patch_jump_to(j, pop_at);
            self.patch_jump(end);
        } else {
            // JumpIfFalse/JumpIfTrue keep the old value and jump straight to
            // the end; the fall-through path writes the new value.
            self.program.emit_op(Opcode::Pop);
            self.emit_value_store(&store, value, keep)?;
            self.patch_jump(j);
        }
        Ok(())
    }

    /// Tail of a logical assignment: evaluate the RHS, keep it if the result
    /// is consumed, and store it through `store`.
    fn emit_value_store(
        &mut self,
        store: &LStore,
        value: &Expr,
        keep: bool,
    ) -> Result<(), CompileError> {
        self.emit_keep(|c| c.emit_expr(value))?;
        if keep {
            self.program.emit_op(Opcode::Dup);
        }
        match store {
            LStore::Local(s) => {
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(*s);
            }
            LStore::Upvalue(i) => {
                self.program.emit_op(Opcode::StoreUpvalue);
                self.program.emit_u8(*i);
            }
            LStore::Global(i) => {
                self.program.emit_op(Opcode::StoreGlobal);
                self.program.emit_u16(*i);
            }
            LStore::Prop(pi, t) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*t);
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(*pi);
                self.program.emit_op(Opcode::SetProperty);
            }
            LStore::Index(to, ti) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*to);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*ti);
                self.program.emit_op(Opcode::SetIndex);
            }
        }
        Ok(())
    }

    fn emit_captures(&mut self, ctx: &FuncCtx) {
        for u in &ctx.upvalues {
            match &u.kind {
                UpvalueKind::Local { slot } => {
                    self.program.emit_op(Opcode::CaptureLocal);
                    self.program.emit_u8(*slot);
                }
                UpvalueKind::Upvalue { index } => {
                    self.program.emit_op(Opcode::CaptureUpvalue);
                    self.program.emit_u8(*index);
                }
                UpvalueKind::Lexical => {
                    // Capture the enclosing frame's `this` or `arguments` at
                    // NewClosure time; WrapCell freezes it in a cell (the
                    // cell, not the raw value, is what NewClosure pops).
                    self.program.emit_op(if u.name == "\u{0}this" {
                        Opcode::LoadThis
                    } else {
                        Opcode::LoadArguments
                    });
                    self.program.emit_op(Opcode::WrapCell);
                }
            }
        }
    }

    fn emit_jump(&mut self, op: Opcode) -> usize {
        self.program.emit_op(op);
        let p = self.program.bytecode.len();
        self.program.emit_u32(0);
        p
    }

    fn patch_jump(&mut self, off: usize) {
        self.patch_jump_to(off, self.program.bytecode.len());
    }

    fn patch_jump_to(&mut self, off: usize, target: usize) {
        let b = (target as u32).to_be_bytes();
        self.program.bytecode[off] = b[0];
        self.program.bytecode[off + 1] = b[1];
        self.program.bytecode[off + 2] = b[2];
        self.program.bytecode[off + 3] = b[3];
    }

    /// Convert an object/array *literal* that appears as the target of `=`
    /// into a destructuring pattern (`[x, y] = arr`, `({ a, b } = obj)`).
    fn expr_to_pattern(e: &Expr) -> Result<Pat, CompileError> {
        match e {
            Expr::Object(fields) => {
                let mut out = Vec::new();
                for f in fields {
                    let (key, value) = match f {
                        ObjElem::Pair(k, v) => (k, v),
                        // Loud errors: computed keys/spreads are legal in JS
                        // assignment targets but unsupported here — better a
                        // compile error than a silent misparse.
                        ObjElem::Computed(..) | ObjElem::Spread(..) => {
                            return Err(CompileError::UnexpectedToken(
                                "computed keys and spreads are not supported in destructuring patterns".to_string(),
                            ));
                        }
                    };
                    let sub = match value {
                        Expr::Ident(name) => Pat::Bind(name.clone()),
                        Expr::Object(_) | Expr::Array(_) => Self::expr_to_pattern(value)?,
                        _ => {
                            return Err(CompileError::UnexpectedToken(
                                "invalid destructuring target".to_string(),
                            ));
                        }
                    };
                    out.push(ObjPatElem::Key(key.clone(), sub));
                }
                Ok(Pat::Object(out))
            }
            Expr::Array(elems) => {
                let mut out = Vec::new();
                for el in elems {
                    if el.hole {
                        out.push(PatElem::Hole);
                        continue;
                    }
                    if el.spread {
                        // `...rest` in an assignment target — must be last.
                        let sub = match &el.expr {
                            Expr::Ident(name) => Pat::Bind(name.clone()),
                            Expr::Object(_) | Expr::Array(_) => Self::expr_to_pattern(&el.expr)?,
                            _ => {
                                return Err(CompileError::UnexpectedToken(
                                    "invalid destructuring target".to_string(),
                                ));
                            }
                        };
                        out.push(PatElem::Rest(sub));
                        if out.len() < elems.len() {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in array pattern".to_string(),
                            ));
                        }
                        continue;
                    }
                    match &el.expr {
                        Expr::Ident(name) => out.push(PatElem::Bind(Pat::Bind(name.clone()))),
                        Expr::Object(_) | Expr::Array(_) => {
                            out.push(PatElem::Bind(Self::expr_to_pattern(&el.expr)?))
                        }
                        _ => {
                            return Err(CompileError::UnexpectedToken(
                                "invalid destructuring target".to_string(),
                            ));
                        }
                    }
                }
                Ok(Pat::Array(out))
            }
            _ => Err(CompileError::UnexpectedToken(
                "invalid destructuring target".to_string(),
            )),
        }
    }

    /// Fuse the string-accumulator assignment `s = s + X` / `s += X` (the
    /// target is a local `s`) into a single dispatch: read the local, add the
    /// RHS (a folded string constant, another local, or the value pushed
    /// after a lhs snapshot — JS evaluation order), store back, and keep the
    /// result when the assignment's value is consumed. Returns true when the
    /// shape matched and the fused code was emitted.
    fn emit_append_assignment(
        &mut self,
        s: u8,
        op: &str,
        value: &Expr,
        name: &str,
        keep: bool,
    ) -> Result<bool, CompileError> {
        // Match the shape: `+=` takes the value as the RHS; `=` requires a
        // binary `+` whose left operand is the target itself.
        let rhs = match op {
            "+=" => Some(value),
            "=" => match value {
                Expr::Bin("+", left, right) => {
                    let mut l = left.as_ref();
                    while let Expr::Paren(inner) = l {
                        l = inner.as_ref();
                    }
                    if let Expr::Ident(ln) = l {
                        if ln == name {
                            Some(right.as_ref())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        };
        let rhs = match rhs {
            Some(r) => r,
            None => return Ok(false),
        };
        let mut r = rhs;
        while let Expr::Paren(inner) = r {
            r = inner.as_ref();
        }
        match r {
            // String literal leaf: fold into the opcode (the accumulator
            // case — one dispatch for the whole append).
            Expr::Str(sl) => {
                let ci = self.program.add_constant(Value::string(sl.clone()));
                self.program.emit_op(Opcode::AppendStringConst);
                self.program.emit_u8(s);
                self.program.emit_u16(ci);
                self.program.emit_u8(keep as u8);
            }
            // Another local: read both inside the opcode.
            Expr::Ident(other) => match self.resolve(other) {
                Resolved::Local(slot) => {
                    self.program.emit_op(Opcode::AppendStringLocal);
                    self.program.emit_u8(s);
                    self.program.emit_u8(slot);
                    self.program.emit_u8(keep as u8);
                }
                _ => return Ok(false),
            },
            // General RHS: take the lhs snapshot first (the accumulator is
            // read before the RHS evaluates, per JS), then the RHS, then the
            // fused pop-store.
            _ => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(s);
                self.emit_keep(|c| c.emit_expr(r))?;
                self.program.emit_op(Opcode::AppendStringPop);
                self.program.emit_u8(s);
                self.program.emit_u8(keep as u8);
            }
        }
        Ok(true)
    }

    /// Store the value on top of the stack into `name`, creating a local (or
    /// REPL global) if needed — the declaration path.
    fn store_declared(&mut self, name: &str) -> Result<(), CompileError> {
        if is_native(name) {
            return Err(CompileError::CannotShadowBuiltin(name.to_string()));
        }
        if self.top_globals && self.funcs.len() == 1 {
            let i = self.global_store_index(name)?;
            self.program.emit_op(Opcode::StoreGlobal);
            self.program.emit_u16(i);
            self.declared_globals.insert(name.to_string());
        } else if let Some(&s) = self.finally_locals.get(name) {
            // Re-emitted finally body: reuse the pre-allocated slot.
            self.program.emit_op(Opcode::StoreLocal);
            self.program.emit_u8(s);
        } else {
            self.funcs.last_mut().unwrap().locals.push(name.to_string());
            let s = (self.funcs.last().unwrap().locals.len() - 1) as u8;
            self.program.emit_op(Opcode::StoreLocal);
            self.program.emit_u8(s);
        }
        Ok(())
    }

    /// Store the value on top of the stack into `name`, resolving to an
    /// existing local/upvalue/global — the assignment path.
    fn store_assign(&mut self, name: &str) -> Result<(), CompileError> {
        match self.resolve(name) {
            Resolved::Local(s) => {
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(s);
            }
            Resolved::Upvalue(u) => {
                self.program.emit_op(Opcode::StoreUpvalue);
                self.program.emit_u8(u);
            }
            Resolved::Global(g) => {
                if is_native(name) {
                    return Err(CompileError::CannotShadowBuiltin(name.to_string()));
                }
                self.program.emit_op(Opcode::StoreGlobal);
                self.program.emit_u16(g);
            }
        }
        Ok(())
    }

    /// Destructure the value stashed in `src_slot` according to `pat`, storing
    /// each bound name. Nested patterns stash their sub-value in a fresh local
    /// before recursing.
    fn emit_pattern_store(&mut self, pat: &Pat, src_slot: u8, mode: PatStoreMode) -> Result<(), CompileError> {
        match pat {
            Pat::Bind(name) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(src_slot);
                match mode {
                    PatStoreMode::Declare => self.store_declared(name)?,
                    PatStoreMode::Assign => self.store_assign(name)?,
                }
            }
            Pat::Object(elems) => {
                // Computed-key VALUES are stashed as they are evaluated so the
                // (final) rest element can delete those keys from its copy
                // without re-evaluating the key expressions.
                let mut computed_slots: Vec<u8> = Vec::new();
                for el in elems {
                    match el {
                        ObjPatElem::Key(key, sub) => {
                            let ki = self.program.add_constant(Value::string(key.clone()));
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::GetProperty);
                            self.program.emit_u16(ki);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                        }
                        ObjPatElem::Computed(key_expr, sub) => {
                            // Evaluate the key ONCE, stash it, then read the
                            // property with the dynamic key.
                            self.emit_expr(key_expr)?;
                            let key_tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(key_tmp);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(key_tmp);
                            self.program.emit_op(Opcode::GetIndex);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                            computed_slots.push(key_tmp);
                        }
                        ObjPatElem::Rest(sub) => {
                            // `...rest`: copy the source's own enumerable
                            // properties into a fresh object, then delete the
                            // already-destructured keys (constant AND computed)
                            // so they stay out of the rest, like JS.
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::MakeObject);
                            self.program.emit_u16(1);
                            self.program.emit_u16(1);
                            let rest_tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(rest_tmp);
                            for key in elems.iter().filter_map(|e| match e {
                                ObjPatElem::Key(k, _) => Some(k),
                                _ => None,
                            }) {
                                let ki = self.program.add_constant(Value::string(key.clone()));
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(rest_tmp);
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.program.emit_op(Opcode::DeleteProp);
                                self.program.emit_op(Opcode::Pop);
                            }
                            for ks in &computed_slots {
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(rest_tmp);
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(*ks);
                                self.program.emit_op(Opcode::DeleteIndex);
                                self.program.emit_op(Opcode::Pop);
                            }
                            self.emit_pattern_store(sub, rest_tmp, mode)?;
                        }
                    }
                }
            }
            Pat::Array(elems) => {
                let mut i = 0usize;
                for el in elems {
                    match el {
                        PatElem::Hole => i += 1,
                        PatElem::Bind(sub) => {
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::LoadInt);
                            self.program.emit_u32(i as u32);
                            self.program.emit_op(Opcode::GetIndex);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                            i += 1;
                        }
                        PatElem::Rest(sub) => {
                            // Collect the source's remaining elements from `i`
                            // onward into a fresh array, then bind it.
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::ArraySlice);
                            self.program.emit_u8(i as u8);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The local slot when `e` is a plain identifier resolving to a local
    /// (used by the property/index superinstruction fusions).
    fn local_slot(&mut self, e: &Expr) -> Option<u8> {
        if let Expr::Ident(name) = e {
            if let Resolved::Local(s) = self.resolve(name) {
                return Some(s);
            }
        }
        None
    }

    /// `local + int` / `local - int` / … — the BinLocalInt shape (`j + 1`).
    /// Returns (slot, arith code, immediate). Only pure arithmetic on a
    /// local and a compile-time int: fusing it into an index write is
    /// unobservable (no side effects in the operands).
    fn local_plus_int(&mut self, e: &Expr) -> Option<(u8, u8, i32)> {
        if let Expr::Bin(op, l, r) = e {
            if let Expr::Int(iv) = r.as_ref() {
                if let Ok(imm) = i32::try_from(*iv) {
                    if let Some(s) = self.local_slot(l) {
                        if let Some(ar) = arith_code(op) {
                            return Some((s, ar, imm));
                        }
                    }
                }
            }
        }
        None
    }

    /// Find the index of a forced class upvalue (`\0home` — the parent class
    /// or the class's prototype) in the current function's capture list.
    /// Absent means `super` was used somewhere it is not legal (outside a
    /// derived-class method/constructor).
    fn find_upvalue(&mut self, name: &str) -> Result<u8, CompileError> {
        self.funcs
            .last()
            .and_then(|f| f.upvalues.iter().position(|u| u.name == name))
            .map(|i| i as u8)
            .ok_or_else(|| {
                CompileError::UnexpectedToken("super used outside a class method".to_string())
            })
    }

    /// Emit one class method as a closure — mirrors the Lambda emission but
    /// starts the function's upvalue list with the forced `\0home` capture
    /// (the class's prototype for instance methods, the parent class for the
    /// constructor of a derived class) so `super` can resolve it by name.
    /// The `home` slot lives in the enclosing frame, so the normal
    /// CaptureLocal machinery materializes it as a cell at NewClosure time.
    fn emit_method(&mut self, m: &MethodDef, home: Option<UpvalueKind>) -> Result<(), CompileError> {
        let j = self.emit_jump(Opcode::Jump);
        let start = self.program.bytecode.len();
        let mut upvalues = Vec::new();
        if let Some(kind) = home {
            upvalues.push(UpvalueRef { name: "\u{0}home".to_string(), kind });
        }
        self.funcs.push(FuncCtx {
            locals: m.params.names(),
            upvalues,
            is_async: m.is_async,
            uses_arguments: false,
            is_arrow: false,
        });
        if let Some(r) = m.params.rest {
            self.program.emit_op(Opcode::MakeRestArray);
            self.program.emit_u8(r as u8);
            self.program.emit_u8(r as u8);
        }
        if m.is_async {
            self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
            let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
            self.program.emit_op(Opcode::NewPromise);
            self.program.emit_u8(ps);
        }
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_trys = std::mem::take(&mut self.trys);
        let saved_finally = std::mem::take(&mut self.finally_locals);
        let saved_labels = std::mem::take(&mut self.labels);
        let saved_pending = self.pending_label.take();
        self.emit_stmt(&m.body)?;
        self.loops = saved_loops;
        self.trys = saved_trys;
        self.finally_locals = saved_finally;
        self.labels = saved_labels;
        self.pending_label = saved_pending;
        self.program.emit_op(Opcode::LoadUndefined);
        self.program.emit_op(Opcode::Return);
        let ctx = self.funcs.pop().unwrap();
        self.patch_jump(j);
        let ci = self.program.add_constant(Value::number(start as f64));
        // Fixed params only: `names` includes the rest param (if any), but
        // the rest slot is materialized by MakeRestArray from the args beyond
        // the fixed params — the missing-arg fill must stop before it (a
        // filled rest slot would become a spurious array element).
        let pcount = m.params.names().len() as u8 - u8::from(m.params.rest.is_some());
        self.emit_captures(&ctx);
        self.program.emit_op(Opcode::NewClosure);
        self.program.emit_u16(ci);
        self.program.emit_u8(ctx.upvalues.len() as u8);
        self.program.emit_u8(pcount);
        self.program.emit_u8(ctx.uses_arguments as u8);
        Ok(())
    }

    /// Any member/call chain containing a `?.` link. Such a chain must be
    /// emitted atomically: the nullish check short-circuits the ENTIRE
    /// remaining chain (later properties, indices, call arguments — none of
    /// them evaluate when the guarded receiver is nullish).
    fn is_optional_chain(e: &Expr) -> bool {
        match e {
            Expr::Paren(inner) => Self::is_optional_chain(inner),
            Expr::Prop { obj, optional, .. } => *optional || Self::is_optional_chain(obj),
            Expr::Index { obj, optional, .. } => *optional || Self::is_optional_chain(obj),
            Expr::Call { callee, optional, .. } => *optional || Self::is_optional_chain(callee),
            _ => false,
        }
    }

    /// Emit `a?.b.c(d)` atomically. The spine is collected (base innermost,
    /// links outward), then emitted with a nullish guard per `?.` link:
    /// `Dup; JumpIfNullish L; Pop` before the link. Every guard jumps to a
    /// short-circuit path AFTER the whole chain, so when the guarded
    /// receiver is nullish the rest of the chain — later members AND call
    /// arguments — never evaluates, exactly like JS. Each skip path discards
    /// the `pushed` stack values the chain accumulated so far and pushes
    /// undefined.
    ///
    /// Method calls (`o?.m()`) keep the receiver below the callee
    /// (`Dup; GetProperty`), so `this` binds correctly on the non-nullish
    /// path. `pushed` counts the extra receiver slots such links leave.
    fn emit_optional_chain(&mut self, e: &Expr) -> Result<(), CompileError> {
        let mut links: Vec<&Expr> = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                Expr::Paren(inner) => cur = inner,
                Expr::Prop { obj, .. } => {
                    links.push(cur);
                    cur = obj;
                }
                Expr::Index { obj, .. } => {
                    links.push(cur);
                    cur = obj;
                }
                Expr::Call { callee, .. } => {
                    links.push(cur);
                    cur = callee;
                }
                base => {
                    self.emit_expr(base)?;
                    break;
                }
            }
        }
        links.reverse();
        let n = links.len();
        // The chain's net stack footprint so far (1 for the value itself,
        // +1 per method-shaped member link that keeps the receiver below).
        let mut pushed = 1usize;
        let mut skips: Vec<(usize, usize)> = Vec::new();
        for (i, link) in links.iter().enumerate() {
            let optional = match link {
                Expr::Prop { optional, .. }
                | Expr::Index { optional, .. }
                | Expr::Call { optional, .. } => *optional,
                _ => unreachable!(),
            };
            if optional {
                // Test a copy, then fall through with the original: the jump
                // opcode consumes the copy, so the chain value stays on the
                // stack for the link below.
                self.program.emit_op(Opcode::Dup);
                let off = self.emit_jump(Opcode::JumpIfNullish);
                skips.push((off, pushed));
            }
            let is_member = matches!(link, Expr::Prop { .. } | Expr::Index { .. });
            let next_is_call = i + 1 < n && matches!(links[i + 1], Expr::Call { .. });
            let method_shape = is_member && next_is_call;
            match link {
                Expr::Prop { prop, .. } => {
                    let pi = self.program.add_constant(Value::string(prop.clone()));
                    if method_shape {
                        // Leave the receiver below the callee for CallMethod.
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        pushed += 1;
                    } else {
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                    }
                }
                Expr::Index { index, .. } => {
                    if method_shape {
                        self.program.emit_op(Opcode::Dup);
                        self.emit_expr(index)?;
                        self.program.emit_op(Opcode::GetIndex);
                        pushed += 1;
                    } else {
                        self.emit_expr(index)?;
                        self.program.emit_op(Opcode::GetIndex);
                    }
                }
                Expr::Call { args, .. } => {
                    // Optional chains always keep the call result (the
                    // short-circuit paths and the outer discard both rely on
                    // the chain leaving exactly one value), so the keep=0
                    // call variants never apply here.
                    let has_spread = args.iter().any(|a| a.spread);
                    let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                        if a.spread { m | (1 << i) } else { m }
                    });
                    let prev_is_member = i > 0
                        && matches!(
                            links[i - 1],
                            Expr::Prop { .. } | Expr::Index { .. }
                        );
                    if prev_is_member {
                        // Method call: the receiver sits one slot below the
                        // frame base (CallMethod reads it as `this`). The
                        // chain already left `[receiver, callee]`; the args
                        // just push on top.
                        for a in args.iter() {
                            self.emit_keep(|c| c.emit_expr(&a.expr))?;
                        }
                        if has_spread {
                            self.program.emit_op(Opcode::CallMethodSpread);
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(Opcode::CallMethod);
                            self.program.emit_u8(args.len() as u8);
                        }
                    } else {
                        // Plain call: Call pops the callee from the TOP, but
                        // the chain value sits below where the args would
                        // land. Stash it in a temp local, emit the args,
                        // then reload it on top so the layout is the plain
                        // `[args..., callee]` convention.
                        let tmp = self.fresh_local();
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(tmp);
                        for a in args.iter() {
                            self.emit_keep(|c| c.emit_expr(&a.expr))?;
                        }
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(tmp);
                        if has_spread {
                            self.program.emit_op(Opcode::CallSpread);
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(Opcode::Call);
                            self.program.emit_u8(args.len() as u8);
                        }
                    }
                    pushed = 1;
                }
                _ => unreachable!(),
            }
        }
        let end = self.emit_jump(Opcode::Jump);
        let mut skip_jumps = Vec::with_capacity(skips.len());
        for (off, pops) in skips {
            self.patch_jump(off);
            for _ in 0..pops {
                self.program.emit_op(Opcode::Pop);
            }
            self.program.emit_op(Opcode::LoadUndefined);
            // Each skip path is a complete branch: jump over the remaining
            // skip paths to the shared end. Without this, a fired guard
            // falls through into the NEXT skip's pops and over-pops the
            // stack (corrupting locals/values below the chain).
            skip_jumps.push(self.emit_jump(Opcode::Jump));
        }
        self.patch_jump(end);
        // All skip paths and the normal path land on the first instruction
        // after the skip block (patch_jump targets the current end).
        for j in skip_jumps {
            self.patch_jump(j);
        }
        Ok(())
    }

    fn emit_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        // Optional chains always emit through the chain walker, even when the
        // top-level node itself isn't optional (`a?.b.c` is a non-optional
        // Prop whose obj spine is optional).
        if Self::is_optional_chain(expr) {
            return self.emit_optional_chain(expr);
        }
        match expr {
            Expr::Paren(inner) => self.emit_expr(inner)?,
            Expr::Sequence(exprs) => {
                // `(a, b, c)`: evaluate each element left-to-right, discard
                // all but the last. In a discard context (keep=false) even
                // the last element goes through the keep-aware stmt path so
                // nothing is left on the stack.
                let last = exprs.len() - 1;
                for (i, e) in exprs.iter().enumerate() {
                    if i == last && self.keep_result {
                        self.emit_expr(e)?;
                    } else {
                        self.emit_stmt_expr(e)?;
                    }
                }
            }
            Expr::Num(n) => {
                let i = self.program.add_constant(Value::number(*n));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(i);
            }
            Expr::Int(v) => {
                if *v >= 0 && *v <= u32::MAX as i64 {
                    self.program.emit_op(Opcode::LoadInt);
                    self.program.emit_u32(*v as u32);
                } else {
                    // Large or negative literals don't fit the 32-bit LoadInt
                    // encoding; fall back to the constant pool. JS numbers are
                    // f64, so an integer literal beyond 2^53 rounds to the
                    // nearest double (`9007199254740993` is 9007199254740992
                    // in V8).
                    let c = if v.unsigned_abs() <= (1u64 << 53) {
                        Value::int(*v)
                    } else {
                        Value::number(*v as f64)
                    };
                    let i = self.program.add_constant(c);
                    self.program.emit_op(Opcode::LoadConst);
                    self.program.emit_u16(i);
                }
            }
            Expr::Str(s) => {
                let i = self.program.add_constant(Value::string(s.clone()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(i);
            }
            Expr::Bool(b) => {
                if *b { self.program.emit_op(Opcode::LoadTrue); }
                else { self.program.emit_op(Opcode::LoadFalse); }
            }
            Expr::Null => self.program.emit_op(Opcode::LoadNull),
            Expr::Undef => self.program.emit_op(Opcode::LoadUndefined),
            Expr::Ident(name) => {
                if name == "this" {
                    // Arrows bind `this` lexically: resolve the hidden
                    // `\0this` capture added at arrow-creation time. Regular
                    // functions read the receiver slot of the current
                    // method/new call (undefined for a plain call). Never a
                    // local.
                    if self.funcs.last().unwrap().is_arrow {
                        if let Some(i) = self.arrow_hidden_index("\u{0}this") {
                            self.program.emit_op(Opcode::LoadUpvalue);
                            self.program.emit_u8(i);
                            return Ok(());
                        }
                    }
                    self.program.emit_op(Opcode::LoadThis);
                    return Ok(());
                }
                if name == "arguments" && self.funcs.len() > 1 {
                    // The call's argument list — array snapshot of the passed
                    // args. A local/param/upvalue named `arguments` shadows
                    // it (JS allows `function f(arguments) {}`); at top level
                    // it falls through to the normal resolve (undefined).
                    let shadowed = self.funcs.last().unwrap().locals.iter().any(|l| l == name)
                        || self.resolve_upvalue(name).is_some();
                    if !shadowed {
                        if self.funcs.last().unwrap().is_arrow {
                            // Arrows have no own `arguments`; read the hidden
                            // `\0arguments` capture (the enclosing binding).
                            if let Some(i) = self.arrow_hidden_index("\u{0}arguments") {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(i);
                                return Ok(());
                            }
                        }
                        // Flag the function so the VM snapshots the passed
                        // args into the frame at entry (locals overwrite the
                        // arg slots as the body runs).
                        self.funcs.last_mut().unwrap().uses_arguments = true;
                        self.program.emit_op(Opcode::LoadArguments);
                        return Ok(());
                    }
                }
                match self.resolve(name) {
                    Resolved::Local(s) => {
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(s);
                    }
                    Resolved::Upvalue(i) => {
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(i);
                    }
                    Resolved::Global(i) => {
                        self.program.emit_op(Opcode::LoadGlobal);
                        self.program.emit_u16(i);
                    }
                }
            }
            Expr::Unary(op, e) => {
                if *op == "void" {
                    // `void expr`: evaluate expr for its side effects, the
                    // result is undefined.
                    self.emit_stmt_expr(e)?;
                    self.program.emit_op(Opcode::LoadUndefined);
                } else if *op == "typeof" {
                    // `typeof undeclaredGlobal` is "undefined", NOT a
                    // ReferenceError. A bare global (through parens) gets the
                    // dedicated TypeOfGlobal opcode; locals/upvalues load
                    // normally (they always exist) and anything else (a
                    // property, a call) loads the value as usual.
                    let mut operand: &Expr = e;
                    while let Expr::Paren(inner) = operand {
                        operand = inner;
                    }
                    if let Expr::Ident(name) = operand {
                        if matches!(self.resolve(name), Resolved::Global(_)) {
                            let i = self.resolve_global(name);
                            self.program.emit_op(Opcode::TypeOfGlobal);
                            self.program.emit_u16(i);
                            return Ok(());
                        }
                    }
                    self.emit_expr(e)?;
                    self.program.emit_op(Opcode::TypeOf);
                } else {
                    self.emit_expr(e)?;
                    match *op {
                        "-" => self.program.emit_op(Opcode::Negate),
                        "~" => self.program.emit_op(Opcode::BitNot),
                        _ => self.program.emit_op(Opcode::Not),
                    }
                }
            }
            Expr::Delete(target) => self.emit_delete(target)?,
            Expr::Bin(op, l, r) => {
                match *op {
                    "&&" => {
                        self.emit_expr(l)?;
                        let j = self.emit_jump(Opcode::JumpIfFalse);
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        self.patch_jump(j);
                    }
                    "||" => {
                        self.emit_expr(l)?;
                        let j = self.emit_jump(Opcode::JumpIfTrue);
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        self.patch_jump(j);
                    }
                    "??" => {
                        // `a ?? b`: the nullish test consumes the dup; the
                        // non-nullish path keeps `a` (already on the stack)
                        // and jumps past, the nullish path replaces it with
                        // `b`. Never evaluates `b` when `a` is not nullish.
                        self.emit_expr(l)?;
                        self.program.emit_op(Opcode::Dup);
                        let j = self.emit_jump(Opcode::JumpIfNullish);
                        let end = self.emit_jump(Opcode::Jump);
                        let pop_at = self.program.bytecode.len();
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        // The nullish path lands on the Pop; the non-nullish
                        // path (which already holds `a`) jumps to the end.
                        self.patch_jump_to(j, pop_at);
                        self.patch_jump(end);
                    }
                    _ => {
                        // Register-ALU fusion: int-arithmetic trees over
                        // locals/literals (`3 * n + 1`, `(lo + hi) % 2`)
                        // collapse into ONE ArithChain dispatch that keeps the
                        // running value in an i64 register. Only chains with
                        // ≥ 2 ops fire — single ops are already fused below.
                        let term = if self.keep_result { 0x80 } else { 0 };
                        if self.emit_arith_chain(op, l, r, term) {
                            return Ok(());
                        }
                        // Fused superinstructions for the hottest shapes: loop
                        // conditions (`i < 1000` -> CmpLocalInt), local/int
                        // arithmetic (`i * 3` -> BinLocalInt), and local/local
                        // arithmetic + comparison (`i + j`, `lo <= hi`).
                        if let (Expr::Ident(ln), Expr::Int(iv)) = (l.as_ref(), r.as_ref()) {
                            if let Resolved::Local(s) = self.resolve(ln) {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    if let Some(cmp) = cmp_code(op) {
                                        self.program.emit_op(Opcode::CmpLocalInt);
                                        self.program.emit_u8(s);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(cmp);
                                        return Ok(());
                                    }
                                    if let Some(ar) = arith_code(op) {
                                        self.program.emit_op(Opcode::BinLocalInt);
                                        self.program.emit_u8(s);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(ar);
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        if let (Expr::Ident(ln), Expr::Ident(rn)) = (l.as_ref(), r.as_ref()) {
                            if let (Resolved::Local(a), Resolved::Local(b)) =
                                (self.resolve(ln), self.resolve(rn))
                            {
                                if let Some(ar) = arith_code(op) {
                                    self.program.emit_op(Opcode::BinLocalLocal);
                                    self.program.emit_u8(a);
                                    self.program.emit_u8(b);
                                    self.program.emit_u8(ar);
                                    return Ok(());
                                }
                                if let Some(cmp) = cmp_code(op) {
                                    self.program.emit_op(Opcode::CmpLocalLocal);
                                    self.program.emit_u8(a);
                                    self.program.emit_u8(b);
                                    self.program.emit_u8(cmp);
                                    return Ok(());
                                }
                            }
                        }
                        self.emit_expr(l)?;
                        self.emit_expr(r)?;
                        match *op {
                            "+" => self.program.emit_op(Opcode::Add),
                            "-" => self.program.emit_op(Opcode::Subtract),
                            "*" => self.program.emit_op(Opcode::Multiply),
                            "/" => self.program.emit_op(Opcode::Divide),
                            "%" => self.program.emit_op(Opcode::Modulo),
                            "==" => self.program.emit_op(Opcode::Equal),
                            "!=" => self.program.emit_op(Opcode::NotEqual),
                            "===" => self.program.emit_op(Opcode::StrictEqual),
                            "!==" => self.program.emit_op(Opcode::StrictNotEqual),
                            "<" => self.program.emit_op(Opcode::Less),
                            ">" => self.program.emit_op(Opcode::Greater),
                            "<=" => self.program.emit_op(Opcode::LessEqual),
                            ">=" => self.program.emit_op(Opcode::GreaterEqual),
                            "&" => self.program.emit_op(Opcode::BitAnd),
                            "|" => self.program.emit_op(Opcode::BitOr),
                            "^" => self.program.emit_op(Opcode::BitXor),
                            "<<" => self.program.emit_op(Opcode::Shl),
                            ">>" => self.program.emit_op(Opcode::Shr),
                            ">>>" => self.program.emit_op(Opcode::UShr),
                            "**" => self.program.emit_op(Opcode::Pow),
                            "instanceof" => self.program.emit_op(Opcode::InstanceOf),
                            "in" => self.program.emit_op(Opcode::In),
                            // A silently-dropped operator would corrupt the
                            // stack (operands left pushed). Every operator the
                            // lexer produces must map to an opcode here.
                            other => {
                                return Err(CompileError::UnexpectedToken(
                                    format!("unhandled binary operator '{}'", other),
                                ));
                            }
                        }
                    }
                }
            }
            Expr::Assign { target, op, value } => {
                // Whether this assignment's result is consumed. False only for
                // a top-level statement assignment (see emit_stmt_expr): the
                // fused ops then get keep=0 and leave nothing, so the
                // statement emits no trailing Pop. Operand sub-expressions are
                // always emitted with keep_result=true (their values feed this
                // assignment), hence the emit_keep scoping.
                // `a?.b = v`, `a?.[k] += v` are SyntaxErrors in JS: an
                // optional chain short-circuits to undefined, so it is not a
                // valid assignment reference.
                if Self::is_optional_chain(target) {
                    return Err(CompileError::UnexpectedToken(
                        "invalid assignment target: optional chaining cannot be assigned".to_string(),
                    ));
                }
                let keep = self.keep_result;
                // `(a) = v` is a valid assignment target: unwrap grouping
                // parens before matching the target shape.
                let mut target_ref = target.as_ref();
                while let Expr::Paren(inner) = target_ref { target_ref = inner.as_ref(); }
                if *op == "&&=" || *op == "||=" || *op == "??=" {
                    if let Expr::Ident(name) = target_ref {
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                    }
                    return self.emit_logical_assign(op, target_ref, value, keep);
                }
                match target_ref {
                    Expr::Ident(name) => {
                        // Assigning to an `import` binding is an ESM
                        // SyntaxError (the live cell aliases the module).
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                        match self.resolve(name) {
                        Resolved::Local(s) => {
                            // `x = <arith chain>` / `x op= <arith chain>`:
                            // fuse the whole read-compute-write into ONE
                            // ArithChain dispatch keeping the int in a
                            // register (store terminal, no stack round-trip).
                            if self.try_emit_chain_assign(s, op, value, keep) {
                                return Ok(());
                            }
                            // `s = s + X` / `s += X`: fuse the whole
                            // read-add-store into one dispatch, keeping the
                            // builder box in the local slot between appends.
                            if self.emit_append_assignment(s, op, value, name, keep)? {
                                return Ok(());
                            }
                            if let Some(ar) = compound_arith(op) {
                                // x op= v: read x, evaluate v (JS order),
                                // then fuse the arith + store into one opcode.
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(s);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(Opcode::ArithStoreLocal);
                                self.program.emit_u8(s);
                                self.program.emit_u8(ar);
                                self.program.emit_u8(keep as u8);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                                if keep {
                                    self.program.emit_op(Opcode::Dup);
                                }
                                self.program.emit_op(Opcode::StoreLocal);
                                self.program.emit_u8(s);
                            }
                        }
                        Resolved::Upvalue(i) => {
                            if let Some(ar) = compound_arith(op) {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(i);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(Opcode::ArithStoreUpvalue);
                                self.program.emit_u8(i);
                                self.program.emit_u8(ar);
                                self.program.emit_u8(keep as u8);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                                if keep {
                                    self.program.emit_op(Opcode::Dup);
                                }
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(i);
                            }
                        }
                        Resolved::Global(i) => {
                            if is_native(name) {
                                return Err(CompileError::CannotShadowBuiltin(name.clone()));
                            }
                            if let Some(opc) = compound_opcode(op) {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(i);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(opc);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                            }
                            // The arith (or the plain RHS) leaves the value on
                            // the stack; keep=0 means StoreGlobal consumes it
                            // directly.
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::StoreGlobal);
                            self.program.emit_u16(i);
                        }
                        }
                    },
                    Expr::Prop { obj, prop, .. } => {
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        if let Some(ar) = compound_arith(op) {
                            // obj.p += v  =>  obj.p = obj.p + v, fused. The obj
                            // stays on the stack and evaluates exactly once; the
                            // old value is read before the RHS, matching JS
                            // compound-assignment order. A constant RHS
                            // collapses the whole read-modify-write into one
                            // CompoundPropConst; otherwise PeekProperty reads
                            // (leaving obj), the RHS evaluates, and
                            // ArithWriteProp writes the result back.
                            let keep_byte = if keep { 16 } else { 0 };
                            if let Expr::Int(iv) = value.as_ref() {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    self.emit_keep(|c| c.emit_expr(obj))?;
                                    self.program.emit_op(Opcode::CompoundPropConst);
                                    self.program.emit_u8(ar | keep_byte);
                                    self.program.emit_u16(pi);
                                    self.program.emit_i32(imm);
                                    return Ok(());
                                }
                            }
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::PeekProperty);
                            self.program.emit_u16(pi);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            self.program.emit_op(Opcode::ArithWriteProp);
                            self.program.emit_u8(ar | keep_byte);
                            self.program.emit_u16(pi);
                            return Ok(());
                        } else {
                            // obj.p = v: evaluate the reference (obj) before
                            // the RHS, per JS. The obj is stashed in a fresh
                            // local so the RHS can run without disturbing it,
                            // then reloaded for the write; the result is the
                            // RHS value.
                            let tmp = self.fresh_local();
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp);
                            self.program.emit_op(Opcode::LoadConst);
                            self.program.emit_u16(pi);
                            self.program.emit_op(Opcode::SetProperty);
                        }
                    }
                    Expr::Index { obj, index, .. } => {
                        if let Some(ar) = compound_arith(op) {
                            // a[i] += v, fused (mirrors the property fusions):
                            // the obj and index stay on the stack and evaluate
                            // exactly once, in JS order; the old value is read
                            // before the RHS. A constant RHS collapses the
                            // read-modify-write into one CompoundIndexConst;
                            // otherwise PeekIndex reads (leaving obj+idx) and
                            // ArithWriteIndex writes back.
                            let keep_byte = if keep { 16 } else { 0 };
                            if let Expr::Int(iv) = value.as_ref() {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    self.emit_keep(|c| c.emit_expr(obj))?;
                                    self.emit_keep(|c| c.emit_expr(index))?;
                                    self.program.emit_op(Opcode::CompoundIndexConst);
                                    self.program.emit_u8(ar | keep_byte);
                                    self.program.emit_i32(imm);
                                    return Ok(());
                                }
                            }
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.emit_keep(|c| c.emit_expr(index))?;
                            self.program.emit_op(Opcode::PeekIndex);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            self.program.emit_op(Opcode::ArithWriteIndex);
                            self.program.emit_u8(ar | keep_byte);
                            return Ok(());
                        } else {
                            // a[i] = v: evaluate the reference (obj, then
                            // index) before the RHS, per JS. Both are stashed
                            // in fresh locals so the RHS can run without
                            // disturbing them; the result is the RHS value.
                            //
                            // Fusion (compiler-side, so operand roles are
                            // unambiguous — the peephole cannot tell a plain
                            // `arr[i] = v` from the stash path's trailing
                            // reloads): when every operand is a pure local
                            // read with no side effects, skipping the stash
                            // is unobservable and the whole write collapses
                            // into ONE dispatch.
                            //   a[i] = v        -> SetIndexLocalLocal
                            //   a[i] = b[j]     -> SetIndexLocalGetLocal
                            //   a[i + k] = b[j] -> SetIndexLocalPlusIntLocalGetLocal
                            let obj_s = self.local_slot(obj);
                            let idx_s = self.local_slot(index);
                            let val_s = self.local_slot(value);
                            if let (Some(os), Some(is)) = (obj_s, idx_s) {
                                if let Some(vs) = val_s {
                                    self.program.emit_op(Opcode::SetIndexLocalLocal);
                                    self.program.emit_u8(os);
                                    self.program.emit_u8(is);
                                    self.program.emit_u8(vs);
                                    if keep {
                                        self.program.emit_op(Opcode::LoadLocal);
                                        self.program.emit_u8(vs);
                                    }
                                    return Ok(());
                                }
                                if let Expr::Index { obj: vobj, index: vidx, .. } = value.as_ref() {
                                    if let (Some(vos), Some(vis)) =
                                        (self.local_slot(vobj), self.local_slot(vidx))
                                    {
                                        self.program.emit_op(Opcode::SetIndexLocalGetLocal);
                                        self.program.emit_u8(os);
                                        self.program.emit_u8(is);
                                        self.program.emit_u8(vos);
                                        self.program.emit_u8(vis);
                                        if keep {
                                            self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                                            self.program.emit_u8(vos);
                                            self.program.emit_u8(vis);
                                        }
                                        return Ok(());
                                    }
                                }
                            }
                            if let (Some(os), Some((is, ar, imm))) =
                                (obj_s, self.local_plus_int(index))
                            {
                                if let Expr::Index { obj: vobj, index: vidx, .. } = value.as_ref() {
                                    if let (Some(vos), Some(vis)) =
                                        (self.local_slot(vobj), self.local_slot(vidx))
                                    {
                                        self.program.emit_op(Opcode::SetIndexLocalPlusIntLocalGetLocal);
                                        self.program.emit_u8(os);
                                        self.program.emit_u8(is);
                                        self.program.emit_u8(ar);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(vos);
                                        self.program.emit_u8(vis);
                                        if keep {
                                            self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                                            self.program.emit_u8(vos);
                                            self.program.emit_u8(vis);
                                        }
                                        return Ok(());
                                    }
                                }
                            }
                            let tmp_obj = self.fresh_local();
                            let tmp_idx = self.fresh_local();
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp_obj);
                            self.emit_keep(|c| c.emit_expr(index))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp_idx);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp_obj);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp_idx);
                            self.program.emit_op(Opcode::SetIndex);
                        }
                    }
                    // Destructuring assignment: `[x, y] = arr` and the
                    // parenthesized `({ a, b } = obj)`. The literal is
                    // converted to a pattern; the RHS is stashed once and every
                    // bound name is stored from it. The result is the RHS.
                    Expr::Object(_) | Expr::Array(_) => {
                        if *op != "=" {
                            return Err(CompileError::UnexpectedToken(
                                "invalid compound assignment target".to_string(),
                            ));
                        }
                        let pat = Self::expr_to_pattern(target)?;
                        self.emit_keep(|c| c.emit_expr(value))?;
                        let tmp = self.fresh_local();
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(tmp);
                        self.emit_pattern_store(&pat, tmp, PatStoreMode::Assign)?;
                        if keep {
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp);
                        }
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken("invalid assignment target".to_string()));
                    }
                }
            }
            Expr::Call { callee, args, .. } => {
                // Whether this call's result is consumed. False only for a
                // top-level statement call (see emit_stmt_expr): the VM then
                // skips pushing the result (CallKeep0), so the statement emits
                // no trailing Pop. The callee and args still evaluate normally
                // and are always emitted with keep_result=true (their values
                // feed the call), hence the emit_keep scoping.
                let keep = self.keep_result;
                let has_spread = args.iter().any(|a| a.spread);
                let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                    if a.spread { m | (1 << i) } else { m }
                });
                // Method calls bind `this`: `o.m(...)` / `o[i](...)` — parens
                // don't strip the reference (`(o.m)()` still binds). The
                // receiver stays below the callee so the frame can read it as
                // its this slot. Note: the member lookup runs before the args
                // (a minor deviation from the spec's after-args order that is
                // unobservable except by an argument that mutates the method).
                let mut c = callee.as_ref();
                while let Expr::Paren(inner) = c {
                    c = inner.as_ref();
                }
                let method = match c {
                    Expr::Prop { obj, prop, .. } => {
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        self.emit_keep(|s| s.emit_expr(obj))?;
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        true
                    }
                    Expr::Index { obj, index, .. } => {
                        self.emit_keep(|s| s.emit_expr(obj))?;
                        self.program.emit_op(Opcode::Dup);
                        self.emit_keep(|s| s.emit_expr(index))?;
                        self.program.emit_op(Opcode::GetIndex);
                        true
                    }
                    _ => false,
                };
                if method {
                    // Stack: [receiver, callee]. Args push on top; CallMethod
                    // reads the receiver from one slot below the frame base.
                    for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                    if has_spread {
                        self.program.emit_op(if keep {
                            Opcode::CallMethodSpread
                        } else {
                            Opcode::CallMethodSpreadKeep0
                        });
                        self.program.emit_u8(args.len() as u8);
                        self.program.emit_u16(mask);
                    } else {
                        self.program.emit_op(if keep {
                            Opcode::CallMethod
                        } else {
                            Opcode::CallMethodKeep0
                        });
                        self.program.emit_u8(args.len() as u8);
                    }
                    return Ok(());
                }
                // Plain call: args first, then the callee (the top of stack
                // is the function the Call opcode pops).
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                self.emit_keep(|c| c.emit_expr(callee))?;
                if has_spread {
                    if keep {
                        self.program.emit_op(Opcode::CallSpread);
                    } else {
                        self.program.emit_op(Opcode::CallSpreadKeep0);
                    }
                    self.program.emit_u8(args.len() as u8);
                    self.program.emit_u16(mask);
                } else {
                    if keep {
                        self.program.emit_op(Opcode::Call);
                    } else {
                        self.program.emit_op(Opcode::CallKeep0);
                    }
                    self.program.emit_u8(args.len() as u8);
                }
            }
            Expr::New { callee, args } => {
                if args.iter().any(|a| a.spread) {
                    return Err(CompileError::UnexpectedToken(
                        "spread in `new` arguments is not supported".to_string(),
                    ));
                }
                // Plain-call convention (args first, callee on top): New
                // pops the callee, inserts the fresh instance below the args,
                // and calls the constructor with `this` bound to it.
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                self.emit_keep(|c| c.emit_expr(callee))?;
                self.program.emit_op(Opcode::New);
                self.program.emit_u8(args.len() as u8);
            }
            Expr::SuperCall { args } => {
                // `super(a)` in a derived class's constructor: call the
                // captured parent class with the current `this` bound.
                let u = self.find_upvalue("\u{0}home")?;
                // [this, parent, args...] — CallMethod layout.
                self.program.emit_op(Opcode::LoadThis);
                self.program.emit_op(Opcode::LoadUpvalue);
                self.program.emit_u8(u);
                let keep = self.keep_result;
                let has_spread = args.iter().any(|a| a.spread);
                let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                    if a.spread { m | (1 << i) } else { m }
                });
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                if has_spread {
                    self.program.emit_op(if keep {
                        Opcode::CallMethodSpread
                    } else {
                        Opcode::CallMethodSpreadKeep0
                    });
                    self.program.emit_u8(args.len() as u8);
                    self.program.emit_u16(mask);
                } else {
                    self.program.emit_op(if keep {
                        Opcode::CallMethod
                    } else {
                        Opcode::CallMethodKeep0
                    });
                    self.program.emit_u8(args.len() as u8);
                }
            }
            Expr::SuperProp { prop, args } => {
                // `super.m(args)` in a method: look `m` up on the home
                // object's parent (the class's proto chain) and call it with
                // the current `this` bound.
                let u = self.find_upvalue("\u{0}home")?;
                let pi = self.program.add_constant(Value::string(prop.clone()));
                match args {
                    Some(args) => {
                        // [this, this, home.proto, m, args...] — CallMethod
                        // reads the receiver from below the frame base.
                        self.program.emit_op(Opcode::LoadThis);
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(u);
                        self.program.emit_op(Opcode::GetProto);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        let keep = self.keep_result;
                        let has_spread = args.iter().any(|a| a.spread);
                        let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                            if a.spread { m | (1 << i) } else { m }
                        });
                        for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                        if has_spread {
                            self.program.emit_op(if keep {
                                Opcode::CallMethodSpread
                            } else {
                                Opcode::CallMethodSpreadKeep0
                            });
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(if keep {
                                Opcode::CallMethod
                            } else {
                                Opcode::CallMethodKeep0
                            });
                            self.program.emit_u8(args.len() as u8);
                        }
                    }
                    None => {
                        // Bare `super.m` reference: the function value only
                        // (no receiver binding).
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(u);
                        self.program.emit_op(Opcode::GetProto);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                    }
                }
            }
            Expr::Class { name: _cname, extends, methods } => {
                // Construction sequence. Synthetic locals in the enclosing
                // frame hold the parent class, the prototype object, and the
                // class (constructor) value; methods capture the prototype (or
                // the parent, for the derived constructor) as their `\0home`.
                let parent_slot = self.fresh_local();
                let proto_slot = self.fresh_local();
                let class_slot = self.fresh_local();
                {
                    let l = self.funcs.last_mut().unwrap();
                    l.locals[parent_slot as usize] = "\u{0}class_parent".into();
                    l.locals[proto_slot as usize] = "\u{0}class_proto".into();
                    l.locals[class_slot as usize] = "\u{0}class_fn".into();
                }
                let has_parent = extends.is_some();
                // 1. Parent value (or undefined).
                match extends {
                    Some(ext) => self.emit_keep(|c| c.emit_expr(ext))?,
                    None => self.program.emit_op(Opcode::LoadUndefined),
                }
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(parent_slot);
                // 2. The prototype object.
                self.program.emit_op(Opcode::MakeObject);
                self.program.emit_u16(0);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(proto_slot);
                // 3. Instance methods → proto.
                for m in methods.iter().filter(|m| m.name != "constructor" && !m.is_static) {
                    let home = if has_parent {
                        Some(UpvalueKind::Local { slot: proto_slot })
                    } else {
                        None
                    };
                    self.emit_method(m, home)?;
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(proto_slot);
                    let pi = self.program.add_constant(Value::string(m.name.clone()));
                    self.program.emit_op(Opcode::LoadConst);
                    self.program.emit_u16(pi);
                    self.program.emit_op(Opcode::SetProperty);
                }
                // 4. The constructor (the class value), stored on the proto
                //    as "constructor". A derived constructor captures the
                //    parent class for `super()`.
                let ctor = methods.iter().find(|m| m.name == "constructor");
                match ctor {
                    Some(m) => {
                        let home = if has_parent {
                            Some(UpvalueKind::Local { slot: parent_slot })
                        } else {
                            None
                        };
                        self.emit_method(m, home)?;
                    }
                    None => {
                        // Default `constructor() {}` — its implicit undefined
                        // return is converted to the instance by the ctor
                        // return rule.
                        let empty = Stmt::Block(Vec::new());
                        self.emit_method(
                            &MethodDef {
                                name: "constructor".into(),
                                is_static: false,
                                is_async: false,
                                kind: MethodKind::Normal,
                                params: FnParams { params: Vec::new(), rest: None },
                                body: Box::new(empty),
                                init: None,
                            },
                            if has_parent {
                                Some(UpvalueKind::Local { slot: parent_slot })
                            } else {
                                None
                            },
                        )?;
                    }
                }
                // The class value IS the constructor: keep a copy on the
                // stack (SetProperty consumes its operands), bind the proto's
                // `constructor` back-reference, then stash the class into its
                // slot.
                self.program.emit_op(Opcode::Dup);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(proto_slot);
                let pi = self.program.add_constant(Value::string("constructor".into()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(pi);
                self.program.emit_op(Opcode::SetProperty);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(class_slot);
                // 5. C.prototype = proto. SetProperty pops [prop, obj, val]
                // with val on top, so the proto (val) is pushed first.
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(proto_slot);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(class_slot);
                let pi = self.program.add_constant(Value::string("prototype".into()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(pi);
                self.program.emit_op(Opcode::SetProperty);
                // 6. extends: proto.proto = parent.prototype.
                if has_parent {
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(proto_slot);
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(parent_slot);
                    let pi = self.program.add_constant(Value::string("prototype".into()));
                    self.program.emit_op(Opcode::GetProperty);
                    self.program.emit_u16(pi);
                    self.program.emit_op(Opcode::SetProto);
                }
                // 7. Static methods → the class function itself. No home
                //    capture: `super` in a static method is rejected at
                //    compile time (static-super needs the class's own proto,
                //    which the function model does not carry).
                for m in methods.iter().filter(|m| m.is_static) {
                    self.emit_method(m, None)?;
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(class_slot);
                    let pi = self.program.add_constant(Value::string(m.name.clone()));
                    self.program.emit_op(Opcode::LoadConst);
                    self.program.emit_u16(pi);
                    self.program.emit_op(Opcode::SetProperty);
                }
                // 8. The class value is the expression result.
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(class_slot);
            }
            Expr::Regex { pattern, flags } => {
                // Fresh regex object per evaluation (own lastIndex). The
                // pattern and flags are string constants; the VM compiles
                // once per (pattern, flags) and caches the program.
                let pi = self.program.add_constant(Value::string(pattern.clone()));
                let fi = self.program.add_constant(Value::string(flags.clone()));
                self.program.emit_op(Opcode::MakeRegex);
                self.program.emit_u16(pi);
                self.program.emit_u16(fi);
            }
            Expr::Index { obj, index, .. } => {
                // `a[i]` with both operands locals fuses into one dispatch
                // (LoadLocal + LoadLocal + GetIndex -> LoadLocalLocalGetIndex).
                if let (Some(os), Some(is)) = (self.local_slot(obj), self.local_slot(index)) {
                    self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                    self.program.emit_u8(os);
                    self.program.emit_u8(is);
                    return Ok(());
                }
                self.emit_expr(obj)?;
                self.emit_expr(index)?;
                self.program.emit_op(Opcode::GetIndex);
            }
            Expr::Array(elems) => {
                for e in elems { self.emit_expr(&e.expr)?; }
                let has_spread = elems.iter().any(|e| e.spread);
                if has_spread {
                    let mask = elems.iter().enumerate().fold(0u16, |m, (i, e)| {
                        if e.spread { m | (1 << i) } else { m }
                    });
                    self.program.emit_op(Opcode::MakeArraySpread);
                    self.program.emit_u16(elems.len() as u16);
                    self.program.emit_u16(mask);
                } else {
                    self.program.emit_op(Opcode::MakeArray);
                    self.program.emit_u16(elems.len() as u16);
                }
            }
            Expr::Prop { obj, prop, .. } => {
                // `local.prop` with a local object and a literal property
                // fuses into one dispatch (LoadLocal + LoadConst +
                // GetProperty -> LoadLocalGetPropConst). The hottest case is
                // `arr.length` in loop conditions.
                if let Some(s) = self.local_slot(obj) {
                    let i = self.program.add_constant(Value::string(prop.clone()));
                    self.program.emit_op(Opcode::LoadLocalGetPropConst);
                    self.program.emit_u8(s);
                    self.program.emit_u16(i);
                    return Ok(());
                }
                self.emit_expr(obj)?;
                let i = self.program.add_constant(Value::string(prop.clone()));
                self.program.emit_op(Opcode::GetProperty);
                self.program.emit_u16(i);
            }
            Expr::Object(fields) => {
                // Constant keys push (key, value) pairs; computed keys push
                // (runtime key, value); spreads push a single value (the
                // mask bit tells MakeObject to expand it in place).
                let mut mask = 0u16;
                for (i, f) in fields.iter().enumerate() {
                    match f {
                        ObjElem::Pair(key, value) => {
                            let ki = self.program.add_constant(Value::string(key.clone()));
                            self.program.emit_op(Opcode::LoadConst);
                            self.program.emit_u16(ki);
                            self.emit_expr(value)?;
                        }
                        ObjElem::Computed(key, value) => {
                            self.emit_expr(key)?;
                            self.emit_expr(value)?;
                        }
                        ObjElem::Spread(src) => {
                            mask |= 1 << i;
                            self.emit_expr(src)?;
                        }
                    }
                }
                self.program.emit_op(Opcode::MakeObject);
                self.program.emit_u16(fields.len() as u16);
                self.program.emit_u16(mask);
            }
            Expr::Template(parts) => {
                // `` `a${x}b` `` compiles to "" + "a" + x + "b", coercing
                // interpolated values to strings via Value::add.
                let empty = self.program.add_constant(Value::string(String::new()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(empty);
                for part in parts {
                    match part {
                        TemplatePart::Lit(s) => {
                            let ci = self.program.add_constant(Value::string(s.clone()));
                            self.program.emit_op(Opcode::LoadConst);
                            self.program.emit_u16(ci);
                        }
                        TemplatePart::Expr(toks) => {
                            let mut p = Parser::new(toks.clone());
                            let e = p.parse_expr(0)?;
                            if !matches!(p.peek(), Token::Eof) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", p.peek())));
                            }
                            self.emit_expr(&e)?;
                        }
                    }
                    self.program.emit_op(Opcode::Add);
                }
            }
            Expr::Lambda { params, body, is_async, is_arrow } => {
                if params.params.iter().any(|p| p.default.is_some()) {
                    return Err(CompileError::UnexpectedToken("default parameters are not yet supported — use explicit `if (x===undefined) x=...` inside the body".to_string()));
                }
                let j = self.emit_jump(Opcode::Jump);
                let start = self.program.bytecode.len();
                // Arrows bind `this`/`arguments` lexically. When the body
                // references them (directly or inside nested arrows), the
                // arrow gets hidden `\0this`/`\0arguments` upvalues: a direct
                // arrow captures the enclosing non-arrow frame's value at
                // creation (kind Lexical -> LoadThis/LoadArguments before
                // NewClosure); a nested arrow re-captures the nearest
                // enclosing arrow's cell (kind Upvalue), so every arrow in a
                // chain shares the same value regardless of how the inner
                // arrows are later called.
                let mut upvalues = Vec::new();
                if *is_arrow {
                    for (hidden, wants) in [
                        ("\u{0}this", stmt_uses_lexical(body, "this")),
                        ("\u{0}arguments", stmt_uses_lexical(body, "arguments")),
                    ] {
                        if !wants {
                            continue;
                        }
                        let kind = self.lexical_capture_kind(hidden);
                        if matches!(kind, UpvalueKind::Lexical) && hidden == "\u{0}arguments" {
                            // The LoadArguments capture runs in the enclosing
                            // function's frame; it needs the args snapshot.
                            self.funcs.last_mut().unwrap().uses_arguments = true;
                        }
                        upvalues.push(UpvalueRef { name: hidden.to_string(), kind });
                    }
                }
                self.funcs.push(FuncCtx {
                    locals: params.names().clone(),
                    upvalues,
                    is_async: *is_async,
                    uses_arguments: false,
                    is_arrow: *is_arrow,
                });
                if let Some(r) = params.rest {
                    // First instruction: materialize the rest array from the
                    // args that exceed the fixed parameters (the operand stack
                    // top is exactly `base + argc` at entry).
                    self.program.emit_op(Opcode::MakeRestArray);
                    self.program.emit_u8(r as u8);
                    self.program.emit_u8(r as u8);
                }
                if *is_async {
                    // Hidden slot (after the params) holds this invocation's
                    // promise; Return resolves it and hands it to the caller.
                    self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
                    let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
                    self.program.emit_op(Opcode::NewPromise);
                    self.program.emit_u8(ps);
                }
                let saved_loops = std::mem::take(&mut self.loops);
                let saved_trys = std::mem::take(&mut self.trys);
                let saved_finally = std::mem::take(&mut self.finally_locals);
                let saved_labels = std::mem::take(&mut self.labels);
                let saved_pending = self.pending_label.take();
                self.emit_stmt(body)?;
                self.loops = saved_loops;
                self.trys = saved_trys;
                self.finally_locals = saved_finally;
                self.labels = saved_labels;
                self.pending_label = saved_pending;
                self.program.emit_op(Opcode::LoadUndefined);
                self.program.emit_op(Opcode::Return);
                let ctx = self.funcs.pop().unwrap();
                self.patch_jump(j);
                let ci = self.program.add_constant(Value::number(start as f64));
                // Fixed params only — `names` includes the rest param, but
                // the rest slot is materialized by MakeRestArray and must not
                // be pre-filled (see emit_method).
                let pcount = params.names().len() as u8 - u8::from(params.rest.is_some());
                self.emit_captures(&ctx);
                self.program.emit_op(Opcode::NewClosure);
                self.program.emit_u16(ci);
                self.program.emit_u8(ctx.upvalues.len() as u8);
                self.program.emit_u8(pcount);
                self.program.emit_u8(ctx.uses_arguments as u8);
            }
            Expr::Await(e) => {
                if !self.funcs.last().unwrap().is_async {
                    return Err(CompileError::AwaitOutsideAsync);
                }
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Await);
            }
            Expr::IncDec { target, is_inc, is_prefix } => {
                // `a?.b++` is a SyntaxError in JS (not a valid reference).
                if Self::is_optional_chain(target) {
                    return Err(CompileError::UnexpectedToken(
                        "invalid update target: optional chaining cannot be incremented".to_string(),
                    ));
                }
                let emit_one = |c: &mut Compiler| {
                    c.program.emit_op(Opcode::LoadInt);
                    c.program.emit_u32(1);
                    if *is_inc {
                        c.program.emit_op(Opcode::Add);
                    } else {
                        c.program.emit_op(Opcode::Subtract);
                    }
                };
                // Whether the inc/dec result is consumed (false only for a
                // top-level statement inc/dec; see emit_stmt_expr).
                let keep = self.keep_result;
                // `x++` evaluates to the old value; `++x` to the new one.
                // `(a)++` is valid: unwrap grouping parens first.
                let mut target_ref = target.as_ref();
                while let Expr::Paren(inner) = target_ref { target_ref = inner.as_ref(); }
                match target_ref {
                    Expr::Ident(name) => {
                        // `++imported` is also an assignment — same loud error.
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                        match self.resolve(name) {
                        Resolved::Local(s) => {
                            // x++ / ++x / x-- / --x on a plain local: one fused
                            // opcode reads, mutates, stores, and pushes (if
                            // keep).
                            let delta = if *is_inc { 1i8 } else { -1i8 };
                            let flags = (*is_prefix as u8) | ((keep as u8) << 1);
                            self.program.emit_op(Opcode::IncLocal);
                            self.program.emit_u8(s);
                            self.program.emit_u8(flags);
                            self.program.emit_u8(delta as u8);
                        }
                        Resolved::Upvalue(u) => {
                            // keep=0: the result is discarded, so both forms
                            // collapse to load / ±1 / store (the Add pushes the
                            // new value, StoreUpvalue consumes it).
                            if !keep {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            } else if *is_prefix {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                emit_one(self);
                                self.program.emit_op(Opcode::Dup);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            } else {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                self.program.emit_op(Opcode::Dup);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            }
                        }
                        Resolved::Global(g) => {
                            if is_native(name) {
                                return Err(CompileError::CannotShadowBuiltin(name.clone()));
                            }
                            if !keep {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            } else if *is_prefix {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                emit_one(self);
                                self.program.emit_op(Opcode::Dup);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            } else {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                self.program.emit_op(Opcode::Dup);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            }
                        }
                        }
                    },
                    Expr::Prop { obj, prop, .. } => {
                        // o.a++ / ++o.a / o.a-- / --o.a: one fused opcode reads
                        // obj.p, adds ±1, writes it back, and pushes the old
                        // (postfix) or new (prefix) value (if keep). The obj
                        // stays on the stack and evaluates exactly once.
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        let flags = (*is_prefix as u8) | ((!*is_inc as u8) << 1) | ((keep as u8) << 2);
                        self.emit_keep(|c| c.emit_expr(obj))?;
                        self.program.emit_op(Opcode::IncPropConst);
                        self.program.emit_u8(flags);
                        self.program.emit_u16(pi);
                    }
                    Expr::Index { obj, index, .. } => {
                        // a[i]++ / ++a[i] / a[i]-- / --a[i]: one fused opcode
                        // reads obj[idx], adds ±1, writes back, and pushes the
                        // old (postfix) or new (prefix) value (if keep). The
                        // obj and index stay on the stack, each evaluating once.
                        let flags = (*is_prefix as u8) | ((!*is_inc as u8) << 1) | ((keep as u8) << 2);
                        self.emit_keep(|c| c.emit_expr(obj))?;
                        self.emit_keep(|c| c.emit_expr(index))?;
                        self.program.emit_op(Opcode::IncIndexConst);
                        self.program.emit_u8(flags);
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken(
                            "invalid increment/decrement target".to_string(),
                        ));
                    }
                }
            }
            Expr::Ternary { cond, then, els } => {
                // cond ? then : else — only the taken branch evaluates, and
                // the condition is discarded, so `&&` / `||` fuse via
                // emit_cond: falsy edges jump to the else branch,
                // truthy-`||` edges jump to the then branch.
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let then_pos = self.program.bytecode.len();
                self.emit_expr(then)?;
                let end = self.emit_jump(Opcode::Jump);
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, then_pos); }
                self.emit_expr(els)?;
                self.patch_jump(end);
            }
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::Expr(e) => self.emit_stmt_expr(e)?,
            Stmt::VarDecl { decls } => {
                for (pat, init) in decls {
                    match init {
                        Some(e) => {
                            // Evaluate the initializer once, stash it, then
                            // bind every name in the pattern from it.
                            self.emit_expr(e)?;
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(pat, tmp, PatStoreMode::Declare)?;
                        }
                        None => match pat {
                            Pat::Bind(name) => {
                                self.program.emit_op(Opcode::LoadUndefined);
                                self.store_declared(name)?;
                            }
                            _ => {
                                return Err(CompileError::UnexpectedToken(
                                    "missing initializer in destructuring declaration".to_string(),
                                ));
                            }
                        },
                    }
                }
            }
            Stmt::FnDecl { name, params, body, is_async } => {
                if is_native(name) {
                    return Err(CompileError::CannotShadowBuiltin(name.clone()));
                }
                if params.params.iter().any(|p| p.default.is_some()) {
                    return Err(CompileError::UnexpectedToken("default parameters are not yet supported — use explicit `if (x===undefined) x=...` inside the body".to_string()));
                }
                let j = self.emit_jump(Opcode::Jump);
                let start = self.program.bytecode.len();
                self.funcs.push(FuncCtx {
                    locals: params.names().clone(),
                    upvalues: Vec::new(),
                    is_async: *is_async,
                    uses_arguments: false,
                    is_arrow: false,
                });
                if let Some(r) = params.rest {
                    self.program.emit_op(Opcode::MakeRestArray);
                    self.program.emit_u8(r as u8);
                    self.program.emit_u8(r as u8);
                }
                // Self slot for recursion: the function value is materialized
                // into this slot at call entry via LoadSelf.
                let name_slot = self.funcs.last().unwrap().locals.len() as u8;
                self.funcs.last_mut().unwrap().locals.push(name.clone());
                self.program.emit_op(Opcode::LoadSelf);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(name_slot);
                if *is_async {
                    // Promise slot after params and the self slot.
                    self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
                    let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
                    self.program.emit_op(Opcode::NewPromise);
                    self.program.emit_u8(ps);
                }
                let saved_loops = std::mem::take(&mut self.loops);
                let saved_trys = std::mem::take(&mut self.trys);
                let saved_finally = std::mem::take(&mut self.finally_locals);
                let saved_labels = std::mem::take(&mut self.labels);
                let saved_pending = self.pending_label.take();
                self.emit_stmt(body)?;
                self.loops = saved_loops;
                self.trys = saved_trys;
                self.finally_locals = saved_finally;
                self.labels = saved_labels;
                self.pending_label = saved_pending;
                self.program.emit_op(Opcode::LoadUndefined);
                self.program.emit_op(Opcode::Return);
                let ctx = self.funcs.pop().unwrap();
                self.patch_jump(j);
                let ci = self.program.add_constant(Value::number(start as f64));
                // Fixed params only — `names` includes the rest param, but
                // the rest slot is materialized by MakeRestArray and must not
                // be pre-filled (see emit_method).
                let pcount = params.names().len() as u8 - u8::from(params.rest.is_some());
                self.emit_captures(&ctx);
                self.program.emit_op(Opcode::NewClosure);
                self.program.emit_u16(ci);
                self.program.emit_u8(ctx.upvalues.len() as u8);
                self.program.emit_u8(pcount);
                self.program.emit_u8(ctx.uses_arguments as u8);
                if self.top_globals && self.funcs.len() == 1 {
                    let i = self.global_store_index(name)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(i);
                } else {
                    // Reuse a hoisted (pre-allocated) slot if present; else
                    // allocate a fresh one.
                    let s = match self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
                        Some(s) => s as u8,
                        None => {
                            self.funcs.last_mut().unwrap().locals.push(name.to_string());
                            (self.funcs.last().unwrap().locals.len() - 1) as u8
                        }
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(s);
                }
            }
            Stmt::Class { name, extends, methods } => {
                // `class C extends B { … }` — build the class value in place
                // (not hoisted, matching JS TDZ semantics), then bind it to
                // the name.
                let e = Expr::Class {
                    name: Some(name.clone()),
                    extends: extends.clone().map(Box::new),
                    methods: methods.to_vec(),
                };
                self.emit_expr(&e)?;
                if self.top_globals && self.funcs.len() == 1 {
                    let i = self.global_store_index(name)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(i);
                } else {
                    let s = match self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
                        Some(s) => s as u8,
                        None => {
                            self.funcs.last_mut().unwrap().locals.push(name.to_string());
                            (self.funcs.last().unwrap().locals.len() - 1) as u8
                        }
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(s);
                }
            }
            Stmt::Return(e) => {
                if let Some(v) = e { self.emit_expr(v)?; }
                else { self.program.emit_op(Opcode::LoadUndefined); }
                if !self.trys.is_empty() {
                    // The finally bodies' locals materialize at their slots,
                    // which could sit where the return value is on the operand
                    // stack. Move the value into a dedicated slot below the
                    // cleanup's locals first, and restore it afterwards.
                    let stash = {
                        let s = self.funcs.last_mut().unwrap().locals.len() as u8;
                        self.funcs.last_mut().unwrap().locals
                            .push("\u{0}retstash".to_string());
                        s
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(stash);
                    // Returning out of enclosing trys runs their finally
                    // bodies first (innermost first).
                    while let Some(ctx) = self.trys.pop() {
                        self.program.emit_op(Opcode::TryEnd);
                        if let Some(f) = ctx.finally {
                            self.emit_finally(&f)?;
                        }
                    }
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(stash);
                }
                self.program.emit_op(Opcode::Return);
            }
            Stmt::If { cond, then, els } => {
                // The condition is discarded and only the taken branch
                // evaluates, so `&&` / `||` fuse via emit_cond:
                // falsy edges jump to the else path, truthy-`||` edges jump to
                // the then branch (the old jump-over-pop dance for the
                // else-less form is gone entirely).
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let then_pos = self.program.bytecode.len();
                self.emit_stmt(then)?;
                if let Some(e) = els {
                    let end = self.emit_jump(Opcode::Jump);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, then_pos); }
                    self.emit_stmt(e)?;
                    self.patch_jump(end);
                } else {
                    let end = self.emit_jump(Opcode::Jump);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, then_pos); }
                    self.patch_jump(end);
                }
            }
            Stmt::While { cond, body } => {
                let ls = self.program.bytecode.len();
                // Inline `&&` / `||` short-circuits: each operand gets its own
                // pop-jump edge (falsy -> exit, truthy-|| -> body) instead of
                // materializing the condition value and popping it.
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let body_pos = self.program.bytecode.len();
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: self.pending_label.take(),
                    is_switch: false,
                });
                self.emit_stmt(body)?;
                let mut lc = self.loops.pop().unwrap();
                self.program.emit_op(Opcode::Jump);
                self.program.emit_u32(ls as u32);
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, body_pos); }
                self.finish_loop(&mut lc, ls);
            }
            Stmt::DoWhile { cond, body } => {
                // `do body while (cond)`: the body runs first, then the
                // condition is tested — truthy jumps back to the body,
                // falsy falls out. `continue` inside the body jumps to the
                // condition, per JS.
                let ls = self.program.bytecode.len();
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: self.pending_label.take(),
                    is_switch: false,
                });
                self.emit_stmt(body)?;
                let cond_pos = self.program.bytecode.len();
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                self.program.emit_op(Opcode::Jump);
                self.program.emit_u32(ls as u32);
                let mut lc = self.loops.pop().unwrap();
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, ls); }
                self.finish_loop(&mut lc, cond_pos);
            }
            Stmt::For { init, cond, update, body } => {
                if let Some(i) = init { self.emit_stmt(i)?; }
                let ls = self.program.bytecode.len();
                if let Some(c) = cond {
                    let mut exit_jumps = Vec::new();
                    let mut body_jumps = Vec::new();
                    self.emit_cond(c, &mut exit_jumps, &mut body_jumps)?;
                    let body_pos = self.program.bytecode.len();
                    self.loops.push(LoopCtx {
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                        continue_target: 0,
                        trys_depth: self.trys.len(),
                        label: self.pending_label.take(),
                        is_switch: false,
                    });
                    self.emit_stmt(body)?;
                    let mut lc = self.loops.pop().unwrap();
                    let update_pos = self.program.bytecode.len();
                    if let Some(u) = update { self.emit_stmt_expr(u)?; }
                    self.program.emit_op(Opcode::Jump);
                    self.program.emit_u32(ls as u32);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, body_pos); }
                    self.finish_loop(&mut lc, update_pos);
                } else {
                    self.loops.push(LoopCtx {
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                        continue_target: 0,
                        trys_depth: self.trys.len(),
                        label: self.pending_label.take(),
                        is_switch: false,
                    });
                    self.emit_stmt(body)?;
                    let mut lc = self.loops.pop().unwrap();
                    let update_pos = self.program.bytecode.len();
                    if let Some(u) = update { self.emit_stmt_expr(u)?; }
                    self.program.emit_op(Opcode::Jump);
                    self.program.emit_u32(ls as u32);
                    self.finish_loop(&mut lc, update_pos);
                }
            }
            Stmt::ForOf { pat, declared, iterable, body } => {
                self.emit_for_iter(pat, *declared, iterable, body, false)?;
            }
            Stmt::ForIn { pat, declared, obj, body } => {
                self.emit_for_iter(pat, *declared, obj, body, true)?;
            }
            Stmt::Block(stmts) => {
                // Function declarations hoist (JS semantics): pre-allocate a
                // slot for every function declared directly in this block, so
                // a function referencing a sibling declared later in the same
                // block (or mutual recursion between siblings) resolves it as
                // a local/upvalue instead of a global. The FnDecl emission
                // reuses the pre-allocated slot via rposition.
                for st in stmts.iter() {
                    if let Stmt::FnDecl { name, .. } = st {
                        if !self.funcs.last().unwrap().locals.contains(name) {
                            self.funcs.last_mut().unwrap().locals.push(name.clone());
                        }
                    }
                }
                // Local slots are intentionally not reclaimed here: truncating
                // would let a later variable reuse a slot that still holds a
                // captured cell, corrupting closure state.
                for st in stmts { self.emit_stmt(st)?; }
            }
            Stmt::Break => self.emit_loop_exit(true, None)?,
            Stmt::Continue => self.emit_loop_exit(false, None)?,
            Stmt::BreakLabel(l) => self.emit_loop_exit(true, Some(l))?,
            Stmt::ContinueLabel(l) => self.emit_loop_exit(false, Some(l))?,
            Stmt::Labeled { name, body } => {
                let is_loop = matches!(
                    body.as_ref(),
                    Stmt::While { .. }
                        | Stmt::For { .. }
                        | Stmt::ForOf { .. }
                        | Stmt::ForIn { .. }
                );
                self.labels.push(LabelCtx {
                    name: name.clone(),
                    is_loop,
                    trys_depth: self.trys.len(),
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                });
                if is_loop {
                    self.pending_label = Some(name.clone());
                }
                self.emit_stmt(body)?;
                let lc = self.labels.pop().unwrap();
                for b in &lc.break_jumps {
                    self.patch_jump(*b);
                }
            }
            Stmt::Throw(e) => {
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Throw);
            }
            Stmt::Try { body, catch, finally } => {
                self.trys.push(TryCtx { finally: finally.clone() });
                let handler_off = self.emit_jump(Opcode::TryStart);
                self.emit_stmt(body)?;
                // Drop this try's context (an exit via break/continue/return
                // already popped it and deactivated the handler; the normal
                // path needs its own TryEnd below).
                self.trys.pop();
                self.program.emit_op(Opcode::TryEnd);
                // Normal completion: run finally, then skip the handler.
                if let Some(f) = finally.as_ref() {
                    self.emit_finally(f)?;
                }
                let end = self.emit_jump(Opcode::Jump);
                // Handler entry: the thrown value is on the stack. Catch/stash
                // slots are allocated HERE (after the try body) so that a
                // nested `catch (e)` inside the body resolves `e` to its own
                // slot: each handler binds the most recently pushed local.
                self.patch_jump_to(handler_off, self.program.bytecode.len());
                let push_slot = |compiler: &mut Compiler, name: &str| -> u8 {
                    let s = compiler.funcs.last_mut().unwrap().locals.len() as u8;
                    compiler.funcs.last_mut().unwrap().locals.push(name.to_string());
                    s
                };
                match (catch.as_ref(), finally.as_ref()) {
                    (Some((name, c)), Some(f)) => {
                        if name.is_empty() {
                            self.program.emit_op(Opcode::Pop);
                        } else {
                            let cs = push_slot(self, name);
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(cs);
                        }
                        // The catch body runs under a finally-protection
                        // handler, so its own throws (and returns/breaks via
                        // the inline emission) still trigger the finally.
                        let fb = self.emit_jump(Opcode::TryStart);
                        self.trys.push(TryCtx { finally: finally.clone() });
                        self.emit_stmt(c)?;
                        self.trys.pop();
                        self.program.emit_op(Opcode::TryEnd);
                        self.emit_finally(f)?;
                        let bdone = self.emit_jump(Opcode::Jump);
                        // The catch body threw: run the finally, then rethrow.
                        self.patch_jump_to(fb, self.program.bytecode.len());
                        let fs = push_slot(self, "\u{0}stash");
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(fs);
                        self.emit_finally(f)?;
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(fs);
                        self.program.emit_op(Opcode::Throw);
                        self.patch_jump(bdone);
                    }
                    (Some((name, c)), None) => {
                        if name.is_empty() {
                            self.program.emit_op(Opcode::Pop);
                        } else {
                            let cs = push_slot(self, name);
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(cs);
                        }
                        self.emit_stmt(c)?;
                    }
                    (None, Some(f)) => {
                        let fs = push_slot(self, "\u{0}stash");
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(fs);
                        self.emit_finally(f)?;
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(fs);
                        self.program.emit_op(Opcode::Throw);
                    }
                    (None, None) => unreachable!(),
                }
                self.patch_jump(end);
            }
            Stmt::Switch { disc, cases } => {
                // The discriminant rides on the operand stack through the
                // dispatch section; each test Dups it, so it survives from
                // one comparison to the next until a match discards it.
                self.emit_expr(disc)?;
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: None,
                    is_switch: true,
                });
                let default_idx = cases.iter().position(|c| c.test.is_none());
                // Dispatch: on a match, drop the discriminant and jump to that
                // arm's body; on a mismatch, keep it and test the next arm.
                let mut arm_jumps = Vec::new();
                for c in cases {
                    if let Some(t) = &c.test {
                        self.program.emit_op(Opcode::Dup);
                        self.emit_expr(t)?;
                        self.program.emit_op(Opcode::StrictEqual);
                        // JumpIfFalse leaves its condition on the stack, so
                        // both paths pop it; the false path keeps the
                        // discriminant for the next test.
                        let next = self.emit_jump(Opcode::JumpIfFalse);
                        self.program.emit_op(Opcode::Pop);
                        self.program.emit_op(Opcode::Pop);
                        arm_jumps.push(self.emit_jump(Opcode::Jump));
                        self.patch_jump(next);
                        self.program.emit_op(Opcode::Pop);
                    }
                }
                // No case matched: drop the discriminant and jump to the
                // default arm, or past the switch.
                self.program.emit_op(Opcode::Pop);
                let end_or_default = match default_idx {
                    Some(_) => self.emit_jump(Opcode::Jump), // patched to default body
                    None => self.emit_jump(Opcode::Jump),    // patched to the end
                };
                // Bodies in source order; an empty arm falls through to the
                // next, giving JS fall-through between consecutive arms.
                let mut arm_offsets = Vec::new();
                for c in cases {
                    arm_offsets.push(self.program.bytecode.len());
                    for s in &c.body {
                        self.emit_stmt(s)?;
                    }
                }
                match default_idx {
                    Some(d) => self.patch_jump_to(end_or_default, arm_offsets[d]),
                    None => self.patch_jump(end_or_default),
                }
                for (i, j) in arm_jumps.iter().enumerate() {
                    self.patch_jump_to(*j, arm_offsets[i]);
                }
                let lc = self.loops.pop().unwrap();
                for b in &lc.break_jumps {
                    self.patch_jump(*b);
                }
            }
            Stmt::Import { src, kind } => match kind {
                ImportKind::Core(names) => {
                    for n in names { self.resolve_global(n); }
                }
                ImportKind::Python(bind) => {
                    // `import { f } from './x.py' as python`: bind the alias
                    // (or first name) to a module object whose properties are
                    // natives for the file's top-level functions, served by
                    // the Python sidecar over the shared segment.
                    if !bind.is_empty() {
                        let idx = self.resolve_global(bind);
                        let ci = self.program.add_constant(Value::string(src.clone()));
                        self.program.emit_op(Opcode::LoadPython);
                        self.program.emit_u16(ci);
                        self.program.emit_op(Opcode::StoreGlobal);
                        self.program.emit_u16(idx);
                    }
                }
                ImportKind::ModuleNamed(pairs) => {
                    // `import { f, g as h } from './x.ajs'`: sugar for
                    // `const m = require(src)` then bind each exported name
                    // to a property of m. Live bindings: GetPropertyCell
                    // returns the RAW cell (the module's own storage), so
                    // the bound global aliases it and LoadGlobal unwraps to
                    // the current value on every read — ESM semantics.
                    self.emit_require_call(src)?;
                    for (exported, local) in pairs {
                        self.imported_bindings.insert(local.clone());
                        let idx = self.resolve_global(local);
                        let ci = self.program.add_constant(Value::string(exported.clone()));
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetPropertyCell);
                        self.program.emit_u16(ci);
                        self.program.emit_op(Opcode::StoreGlobal);
                        self.program.emit_u16(idx);
                    }
                    self.program.emit_op(Opcode::Pop);
                }
                ImportKind::ModuleNamespace(bind) => {
                    // `import * as m`: m aliases the whole exports object,
                    // whose properties are the module's cells — reads of
                    // `m.x` are live. Reassigning m is an ESM SyntaxError.
                    self.imported_bindings.insert(bind.clone());
                    let idx = self.resolve_global(bind);
                    self.emit_require_call(src)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                }
                ImportKind::ModuleDefault(bind) => {
                    // `import d from`: the default export is a value
                    // (ESM's default import is not a live binding), so the
                    // ordinary snapshot read is correct.
                    self.imported_bindings.insert(bind.clone());
                    let idx = self.resolve_global(bind);
                    self.emit_require_call(src)?;
                    let ci = self.program.add_constant(Value::string("default".to_string()));
                    self.program.emit_op(Opcode::GetProperty);
                    self.program.emit_u16(ci);
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                }
                ImportKind::ModuleSideEffect => {
                    self.emit_require_call(src)?;
                    self.program.emit_op(Opcode::Pop);
                }
            },
            Stmt::Export { pairs, stmt, default } => {
                if !self.in_module {
                    // Node also rejects `export` outside an ES module.
                    return Err(CompileError::UnexpectedToken(
                        "'export' is only allowed in a module (a file loaded with require())"
                            .to_string(),
                    ));
                }
                // `export default expr`: evaluate the expression and store it
                // under the reserved name, then record the export.
                if *default {
                    let idx = self.resolve_global(DEFAULT_EXPORT);
                    if let Stmt::Expr(e) = &**stmt {
                        self.emit_expr(e)?;
                    }
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                } else {
                    self.emit_stmt(stmt)?;
                }
                for (name, binding) in pairs {
                    if !self.program.exports.iter().any(|(n, _)| n == name) {
                        self.program.exports.push((name.clone(), binding.clone()));
                    }
                }
            }
            Stmt::Nop => {}
        }
        Ok(())
    }

    /// Emit `for (name of/iterable)` as an indexed while-loop, so break and
    /// continue inside the body behave like any other loop. `is_in` iterates
    /// object keys instead of array elements.
    fn emit_for_iter(
        &mut self,
        pat: &Pat,
        declared: bool,
        source: &Expr,
        body: &Stmt,
        is_in: bool,
    ) -> Result<(), CompileError> {
        let arr = self.fresh_name("arr");
        let idx = self.fresh_name("i");
        let len = self.fresh_name("len");
        for t in [&arr, &idx, &len] {
            self.funcs.last_mut().unwrap().locals.push(t.clone());
        }
        let arr_slot = (self.funcs.last().unwrap().locals.len() - 3) as u8;
        let idx_slot = (self.funcs.last().unwrap().locals.len() - 2) as u8;
        let len_slot = (self.funcs.last().unwrap().locals.len() - 1) as u8;

        // arr = source (or Object.keys(source) for for-in). A for-of over a
        // Map/Set first converts the container to its iteration snapshot
        // (entry pairs / elements) — the synthetic iterator — so the index
        // loop below iterates it like any array.
        self.emit_expr(source)?;
        if is_in {
            self.program.emit_op(Opcode::GetKeys);
        } else {
            self.program.emit_op(Opcode::ToIterable);
        }
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(arr_slot);

        // idx = 0
        self.program.emit_op(Opcode::LoadInt);
        self.program.emit_u32(0);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(idx_slot);

        // len = arr.length (GetProperty reads its prop from the operand)
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(arr_slot);
        let lc = self.program.add_constant(Value::string("length".to_string()));
        self.program.emit_op(Opcode::GetProperty);
        self.program.emit_u16(lc);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(len_slot);

        // while idx < len
        let ls = self.program.bytecode.len();
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(len_slot);
        self.program.emit_op(Opcode::Less);
        let ej = self.emit_jump(Opcode::JumpIfFalsePop);

        // Bind the loop pattern from arr[idx]: stash the element once, then
        // destructure it (a plain name stores directly, patterns recurse).
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(arr_slot);
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::GetIndex);
        let tmp = self.fresh_local();
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(tmp);
        let mode = if declared { PatStoreMode::Declare } else { PatStoreMode::Assign };
        self.emit_pattern_store(pat, tmp, mode)?;

        self.loops.push(LoopCtx {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: 0,
            trys_depth: self.trys.len(),
            label: self.pending_label.take(),
            is_switch: false,
        });
        self.emit_stmt(body)?;
        let mut lc = self.loops.pop().unwrap();

        // idx += 1
        let update_pos = self.program.bytecode.len();
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::LoadInt);
        self.program.emit_u32(1);
        self.program.emit_op(Opcode::Add);
        self.program.emit_op(Opcode::Dup);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::Pop);

        self.program.emit_op(Opcode::Jump);
        self.program.emit_u32(ls as u32);
        self.patch_jump(ej);
        self.finish_loop(&mut lc, update_pos);
        Ok(())
    }

    /// Patch a loop's break/continue jumps once the loop is fully emitted.
    /// `continue_target` is the bytecode offset the loop's `continue` jumps to
    /// (the condition for `while`, the update expression for `for`/for-of).
    /// If the loop carries a label, its `continue` jumps are patched too.
    fn finish_loop(&mut self, lc: &mut LoopCtx, continue_target: usize) {
        lc.continue_target = continue_target;
        for b in &lc.break_jumps {
            self.patch_jump(*b);
        }
        for c in &lc.continue_jumps {
            self.patch_jump_to(*c, continue_target);
        }
        if let Some(l) = lc.label.take() {
            if let Some(li) = self.labels.iter().rposition(|x| x.name == l) {
                self.labels[li].continue_target = continue_target;
                let jumps = std::mem::take(&mut self.labels[li].continue_jumps);
                for c in jumps {
                    self.patch_jump_to(c, continue_target);
                }
            }
        }
    }

    /// Emit the machinery for `break` (is_break = true) or `continue` out of a
    /// loop, optionally targeting a label. Runs the finallys of trys between
    /// the exit site and the target (but never trys enclosing the target), and
    /// records the jump for patching at the target's end.
    fn emit_loop_exit(&mut self, is_break: bool, label: Option<&str>) -> Result<(), CompileError> {
        let trys_depth;
        let target: ExitTarget;
        if let Some(l) = label {
            let idx = self.labels.iter().rposition(|lc| lc.name == l)
                .ok_or_else(|| CompileError::UndefinedLabel(l.to_string()))?;
            if !is_break && !self.labels[idx].is_loop {
                return Err(CompileError::ContinueNonLoop(l.to_string()));
            }
            trys_depth = self.labels[idx].trys_depth;
            target = ExitTarget::Label(idx);
        } else if is_break {
            // `break` targets the innermost break target (loop or switch).
            if self.loops.is_empty() {
                return Err(CompileError::BreakOutsideLoop);
            }
            let idx = self.loops.len() - 1;
            trys_depth = self.loops[idx].trys_depth;
            target = ExitTarget::Loop(idx);
        } else {
            // `continue` targets the innermost *loop*; switches are break
            // targets only, so an unlabeled `continue` skips past them to the
            // enclosing loop (or errors at the top level).
            match self.loops.iter().rposition(|lc| !lc.is_switch) {
                Some(idx) => {
                    trys_depth = self.loops[idx].trys_depth;
                    target = ExitTarget::Loop(idx);
                }
                None => return Err(CompileError::BreakOutsideLoop),
            }
        }
        while self.trys.len() > trys_depth {
            let ctx = self.trys.pop().unwrap();
            self.program.emit_op(Opcode::TryEnd);
            if let Some(f) = ctx.finally {
                self.emit_finally(&f)?;
            }
        }
        let j = self.emit_jump(Opcode::Jump);
        match target {
            ExitTarget::Loop(idx) => {
                if is_break {
                    self.loops[idx].break_jumps.push(j);
                } else {
                    self.loops[idx].continue_jumps.push(j);
                }
            }
            ExitTarget::Label(i) => {
                if is_break {
                    self.labels[i].break_jumps.push(j);
                } else {
                    self.labels[i].continue_jumps.push(j);
                }
            }
        }
        Ok(())
    }

    pub fn compile_source(source: &str) -> Result<Program, CompileError> {
        Self::compile_source_with_mode(source, false, false)
    }

    /// Compile a module file (loaded by `require`): top-level declarations
    /// become globals (so exports are readable after the file finishes), and
    /// `export` declarations are legal and recorded on the program.
    pub fn compile_module(source: &str) -> Result<Program, CompileError> {
        let mut p = Self::compile_source_with_mode(source, true, true)?;
        p.is_module = true;
        Ok(p)
    }

    pub fn compile_source_with_mode(
        source: &str,
        top_level_globals: bool,
        in_module: bool,
    ) -> Result<Program, CompileError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_program()?;
        let mut c = Compiler::new();
        c.top_globals = top_level_globals;
        c.in_module = in_module;
        // Hoist top-level function declarations too, so mutual recursion
        // between them resolves each name to its local slot. (Only when
        // top-level declarations stay locals: in top-globals/REPL mode the
        // FnDecl emitter stores them as globals, so hoisting them into locals
        // would make call sites load an uninitialized slot.)
        if !top_level_globals {
            for s in &ast {
                if let Stmt::FnDecl { name, .. } = s {
                    if !c.funcs[0].locals.contains(name) {
                        c.funcs[0].locals.push(name.clone());
                    }
                }
            }
        }
        // Constants (string/array/object literals) allocate into the
        // program's own arena while emitting.
        let heap_ptr: *mut ArenaHeap = &mut c.program.heap;
        let _g = HeapGuard::set(heap_ptr);
        for s in &ast {
            c.emit_stmt(s)?;
        }
        // Every export binding must be declared somewhere in the module
        // (Node: "Export 'x' is not defined" at link time). Only checked in
        // module mode — ordinary scripts already reject `export` entirely.
        if c.in_module {
            for s in &ast {
                if let Stmt::Export { pairs, .. } = s {
                    for (_, binding) in pairs {
                        if binding != DEFAULT_EXPORT
                            && !c.program.globals.contains(binding)
                        {
                            return Err(CompileError::UndefinedVariable(
                                binding.clone(),
                            ));
                        }
                    }
                }
            }
        }
        c.program.emit_op(Opcode::Halt);
        // Fuse the hottest stack-op sequences the AST-level fusion misses
        // (`3 * n`, `(i + j) % 7`, constant folds), rebasing jump targets.
        c.program.peephole();
        // Reclaim the transient constant allocations (failed interns,
        // intermediate strings) and intern the surviving constants into the
        // program's own old generation.
        c.program.compact_constants();
        Ok(c.program)
    }

    /// Whether `src` looks like a complete unit of input: brackets balanced and
    /// no unterminated string, template literal, or comment. The REPL uses this
    /// to decide whether to evaluate a line now or keep reading continuation
    /// lines. A *mismatched* closing bracket counts as complete (the compiler
    /// will report it) so the REPL never buffers forever on bad input.
    pub fn input_is_complete(src: &str) -> bool {
        let chars: Vec<char> = src.chars().collect();
        let mut i = 0usize;
        let mut delims: Vec<char> = Vec::new(); // ( [ {
        enum Mode {
            Code,
            Str(char),
            Tmpl,
            /// Inside a template's `${ ... }` interpolation.
            TmplExpr,
            LineComment,
            BlockComment,
        }
        let mut mode = Mode::Code;
        let mut tmpl_expr_depth = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match &mode {
                Mode::LineComment => {
                    if c == '\n' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
                Mode::BlockComment => {
                    if c == '*' && chars.get(i + 1) == Some(&'/') {
                        mode = Mode::Code;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::Str(q) => {
                    if c == '\\' {
                        i += 2;
                    } else if c == *q {
                        mode = Mode::Code;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                Mode::Tmpl => {
                    if c == '\\' {
                        i += 2;
                    } else if c == '`' {
                        mode = Mode::Code;
                        i += 1;
                    } else if c == '$' && chars.get(i + 1) == Some(&'{') {
                        tmpl_expr_depth = 1;
                        mode = Mode::TmplExpr;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::TmplExpr => {
                    match c {
                        '{' => {
                            tmpl_expr_depth += 1;
                            i += 1;
                        }
                        '}' => {
                            tmpl_expr_depth -= 1;
                            if tmpl_expr_depth == 0 {
                                mode = Mode::Tmpl;
                            }
                            i += 1;
                        }
                        '"' | '\'' => {
                            mode = Mode::Str(c);
                            i += 1;
                        }
                        '`' => {
                            // Nested template inside an interpolation.
                            mode = Mode::Tmpl;
                            i += 1;
                        }
                        '/' => {
                            if chars.get(i + 1) == Some(&'/') {
                                mode = Mode::LineComment;
                                i += 2;
                            } else if chars.get(i + 1) == Some(&'*') {
                                mode = Mode::BlockComment;
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                }
                Mode::Code => {
                    match c {
                        '(' | '[' | '{' => {
                            delims.push(c);
                            i += 1;
                        }
                        ')' | ']' | '}' => {
                            let want = if c == ')' { '(' } else if c == ']' { '[' } else { '{' };
                            if delims.last() == Some(&want) {
                                delims.pop();
                            } else {
                                // Mismatched: complete, so the compiler can
                                // report the error instead of buffering forever.
                                return true;
                            }
                            i += 1;
                        }
                        '"' | '\'' => {
                            mode = Mode::Str(c);
                            i += 1;
                        }
                        '`' => {
                            mode = Mode::Tmpl;
                            i += 1;
                        }
                        '/' => {
                            if chars.get(i + 1) == Some(&'/') {
                                mode = Mode::LineComment;
                                i += 2;
                            } else if chars.get(i + 1) == Some(&'*') {
                                mode = Mode::BlockComment;
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                }
            }
        }
        // A line comment runs to the end of input, so it doesn't make the
        // unit incomplete; an unterminated block comment does.
        delims.is_empty() && matches!(mode, Mode::Code | Mode::LineComment)
    }
}
