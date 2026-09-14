use crate::compiler::error::CompileError;
use crate::compiler::token::{TemplatePart, Token, TokenStream};

pub struct Lexer {
    src: Vec<char>,
    pos: usize,
    /// `line_of[i]` = the 1-based source line of the char at index `i` (one
    /// extra entry so `pos == src.len()` at Eof is valid).
    line_of: Vec<u32>,
    col_of: Vec<u32>,
    tokens: Vec<Token>,
    lines: Vec<u32>,
    cols: Vec<u32>,
}

impl Lexer {
    pub fn new(s: &str) -> Self {
        let src: Vec<char> = s.chars().collect();
        let mut line_of = vec![1u32; src.len() + 1];
        let mut col_of = vec![1u32; src.len() + 1];
        let mut line = 1u32;
        let mut col = 1u32;
        for (i, &c) in src.iter().enumerate() {
            line_of[i] = line;
            col_of[i] = col;
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        line_of[src.len()] = line;
        col_of[src.len()] = col;
        Self {
            src,
            pos: 0,
            line_of,
            col_of,
            tokens: Vec::new(),
            lines: Vec::new(),
            cols: Vec::new(),
        }
    }

    pub fn cur_line(&self) -> u32 {
        self.line_of.get(self.pos).copied().unwrap_or(1)
    }

    pub fn cur_col(&self) -> u32 {
        self.col_of.get(self.pos).copied().unwrap_or(1)
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn adv(&mut self) -> Option<char> {
        let c = self.src.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
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
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
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
        if self.peek() == Some('n') {
            return Err(CompileError::InvalidNumber(
                "BigInt literals are not supported".to_string(),
            ));
        }
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
                Err(_) => Ok(Token::Number(s.parse().unwrap_or(0.0))),
            }
        }
    }

    fn read_radix_num(&mut self, radix: u32) -> Result<Token, CompileError> {
        let digits: &str = match radix {
            16 => "0123456789abcdef",
            2 => "01",
            _ => "01234567",
        };
        self.adv(); // consume x / b / o
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch == 'n' {
                break;
            }
            if ch.is_ascii_alphanumeric() || ch == '_' {
                if digits.contains(ch.to_ascii_lowercase()) {
                    s.push(ch);
                    self.adv();
                } else {
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
        if self.peek() == Some('n') {
            return Err(CompileError::InvalidNumber(
                "BigInt literals are not supported".to_string(),
            ));
        }
        match u128::from_str_radix(&s, radix) {
            Ok(v) if v <= i64::MAX as u128 => Ok(Token::Int(v as i64)),
            Ok(v) => Ok(Token::Number(v as f64)),
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
            "yield" => Token::Yield,
            _ => Token::Ident(s),
        }
    }

    fn push_tok(&mut self, start: usize, tok: Token) {
        self.lines.push(self.line_of[start]);
        self.cols.push(self.col_of[start]);
        self.tokens.push(tok);
    }

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

    pub fn tokenize(&mut self) -> Result<TokenStream, CompileError> {
        loop {
            self.skip_ws();
            let start = self.pos;
            let ch = match self.peek() {
                None => {
                    self.push_tok(start, Token::Eof);
                    break;
                }
                Some(c) => c,
            };
            match ch {
                '(' => {
                    self.adv();
                    self.push_tok(start, Token::LParen);
                }
                ')' => {
                    self.adv();
                    self.push_tok(start, Token::RParen);
                }
                '{' => {
                    self.adv();
                    self.push_tok(start, Token::LBrace);
                }
                '}' => {
                    self.adv();
                    self.push_tok(start, Token::RBrace);
                }
                '[' => {
                    self.adv();
                    self.push_tok(start, Token::LBracket);
                }
                ']' => {
                    self.adv();
                    self.push_tok(start, Token::RBracket);
                }
                ';' => {
                    self.adv();
                    self.push_tok(start, Token::Semicolon);
                }
                ',' => {
                    self.adv();
                    self.push_tok(start, Token::Comma);
                }
                ':' => {
                    self.adv();
                    self.push_tok(start, Token::Colon);
                }
                '?' => {
                    self.adv();
                    if self.peek() == Some('.')
                        && !self
                            .src
                            .get(self.pos + 1)
                            .map(|c| c.is_ascii_digit())
                            .unwrap_or(false)
                    {
                        self.adv();
                        self.push_tok(start, Token::QuestionDot);
                    } else if self.peek() == Some('?') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::NullishAssign);
                        } else {
                            self.push_tok(start, Token::QuestionQuestion);
                        }
                    } else {
                        self.push_tok(start, Token::Question);
                    }
                }
                '.' => {
                    self.adv();
                    if self.peek() == Some('.') && self.src.get(self.pos + 1) == Some(&'.') {
                        self.adv();
                        self.adv();
                        self.push_tok(start, Token::DotDotDot);
                    } else if self.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        let num = self.read_num('.')?;
                        self.push_tok(start, num);
                    } else {
                        self.push_tok(start, Token::Dot);
                    }
                }
                '+' => {
                    self.adv();
                    if self.peek() == Some('+') {
                        self.adv();
                        self.push_tok(start, Token::PlusPlus);
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::PlusAssign);
                    } else {
                        self.push_tok(start, Token::Plus);
                    }
                }
                '-' => {
                    self.adv();
                    if self.peek() == Some('-') {
                        self.adv();
                        self.push_tok(start, Token::MinusMinus);
                    } else if self.peek() == Some('>') {
                        self.adv();
                        self.push_tok(start, Token::Arrow);
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::MinusAssign);
                    } else {
                        self.push_tok(start, Token::Minus);
                    }
                }
                '*' => {
                    self.adv();
                    if self.peek() == Some('*') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::StarStarAssign);
                        } else {
                            self.push_tok(start, Token::StarStar);
                        }
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::StarAssign);
                    } else {
                        self.push_tok(start, Token::Star);
                    }
                }
                '/' => {
                    if self.regex_allowed(self.tokens.last()) {
                        let (pattern, flags) = self.read_regex()?;
                        self.push_tok(start, Token::Regex { pattern, flags });
                    } else {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::SlashAssign);
                        } else {
                            self.push_tok(start, Token::Slash);
                        }
                    }
                }
                '%' => {
                    self.adv();
                    if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::PercentAssign);
                    } else {
                        self.push_tok(start, Token::Percent);
                    }
                }
                '=' => {
                    self.adv();
                    if self.peek() == Some('>') {
                        self.adv();
                        self.push_tok(start, Token::Arrow);
                    } else if self.peek() == Some('=') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::EqEqEq);
                        } else {
                            self.push_tok(start, Token::EqEq);
                        }
                    } else {
                        self.push_tok(start, Token::Assign);
                    }
                }
                '!' => {
                    self.adv();
                    if self.peek() == Some('=') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::NeqEq);
                        } else {
                            self.push_tok(start, Token::Neq);
                        }
                    } else {
                        self.push_tok(start, Token::Not);
                    }
                }
                '<' => {
                    self.adv();
                    if self.peek() == Some('<') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::ShlAssign);
                        } else {
                            self.push_tok(start, Token::Shl);
                        }
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::Lte);
                    } else {
                        self.push_tok(start, Token::Lt);
                    }
                }
                '>' => {
                    self.adv();
                    if self.peek() == Some('>') {
                        self.adv();
                        if self.peek() == Some('>') {
                            self.adv();
                            if self.peek() == Some('=') {
                                self.adv();
                                self.push_tok(start, Token::UShrAssign);
                            } else {
                                self.push_tok(start, Token::UShr);
                            }
                        } else if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::ShrAssign);
                        } else {
                            self.push_tok(start, Token::Shr);
                        }
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::Gte);
                    } else {
                        self.push_tok(start, Token::Gt);
                    }
                }
                '&' => {
                    self.adv();
                    if self.peek() == Some('&') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::AndAssign);
                        } else {
                            self.push_tok(start, Token::And);
                        }
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::BitAndAssign);
                    } else {
                        self.push_tok(start, Token::BitAnd);
                    }
                }
                '|' => {
                    self.adv();
                    if self.peek() == Some('|') {
                        self.adv();
                        if self.peek() == Some('=') {
                            self.adv();
                            self.push_tok(start, Token::OrAssign);
                        } else {
                            self.push_tok(start, Token::Or);
                        }
                    } else if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::BitOrAssign);
                    } else {
                        self.push_tok(start, Token::BitOr);
                    }
                }
                '^' => {
                    self.adv();
                    if self.peek() == Some('=') {
                        self.adv();
                        self.push_tok(start, Token::BitXorAssign);
                    } else {
                        self.push_tok(start, Token::BitXor);
                    }
                }
                '~' => {
                    self.adv();
                    self.push_tok(start, Token::BitNot);
                }
                '"' | '\'' => {
                    self.adv();
                    let s = self.read_str(ch)?;
                    self.push_tok(start, Token::StringLit(s));
                }
                '`' => {
                    self.adv();
                    let tpl = self.read_template()?;
                    self.push_tok(start, tpl);
                }
                c if c.is_ascii_digit() => {
                    self.adv();
                    let num = self.read_num(c)?;
                    self.push_tok(start, num);
                }
                c if c.is_alphabetic() || c == '_' || c == '$' => {
                    self.adv();
                    let ident = self.read_ident(c);
                    self.push_tok(start, ident);
                }
                '#' => {
                    self.adv();
                    let mut s = String::from("#");
                    if let Some(c) = self.peek() {
                        if c.is_alphabetic() || c == '_' || c == '$' {
                            s.push(c);
                            self.adv();
                            while let Some(ch) = self.peek() {
                                if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                                    s.push(ch);
                                    self.adv();
                                } else {
                                    break;
                                }
                            }
                            self.push_tok(start, Token::PrivateIdent(s));
                        }
                    }
                }
                _ => {
                    self.adv();
                }
            }
        }
        Ok(TokenStream {
            tokens: std::mem::take(&mut self.tokens),
            lines: std::mem::take(&mut self.lines),
            cols: std::mem::take(&mut self.cols),
        })
    }
}
