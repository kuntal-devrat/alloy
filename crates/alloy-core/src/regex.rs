//! A small backtracking regex engine powering regex literals (`/ab+c/i`).
//!
//! Dependency-free on purpose — the engine is a classic backtracking VM
//! (split/jump/save/match, like Cox's re2 backtracker): pattern → bytecode →
//! depth-first search over `Vec<char>`. Supported surface:
//!
//! - literals, `.`, `^`/`$` (with `m` line anchors), `[...]` classes with
//!   ranges and negation, `\d \D \w \W \s \S`, `\b`/`\B`, escapes (`\n \t \r
//!   \f \v \0`, `\xHH`, `\uHHHH`, `\cX`, escaped punctuation)
//! - groups `(...)` with captures, non-capturing `(?:...)`, alternation `|`,
//!   backreferences `\1`…`\9`
//! - quantifiers `* + ? {n} {n,} {n,m}` plus lazy variants (`*?` etc.)
//! - flags `g i m s y u` (parsed and validated; `i` folds ASCII, `u` is
//!   accepted for compatibility and treated as ASCII semantics; `y` anchors
//!   matching at the search start)
//!
//! Deliberate limits, all loud: lookahead/lookbehind (`(?=…)`, `(?!…)`,
//! `(?<=…)`) and named groups are rejected at compile time; `\p{…}` unicode
//! property escapes are rejected; `i`-case-folding is ASCII-only; `.` and
//! classes operate on code points (a surrogate pair is one `.` here, two in
//! V8's UTF-16 model) — divergences only for non-BMP or non-ASCII-case text.
//!
//! Matching is over **chars**; reported positions are converted to UTF-16
//! code-unit offsets by the caller, matching V8's `.index`/`lastIndex`
//! arithmetic for the common BMP case.

/// Regex flags. Parsed from the literal's flag string with validation
/// (unknown or duplicate flags are an error, like Node).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegexFlags {
    pub global: bool,
    pub ignore_case: bool,
    pub multiline: bool,
    pub dot_all: bool,
    pub sticky: bool,
    /// Accepted and validated, without full Unicode semantics (the engine is
    /// ASCII-oriented; `u` changes little beyond Node's error surface).
    pub unicode: bool,
}

impl RegexFlags {
    pub fn parse(s: &str) -> Result<RegexFlags, String> {
        let mut f = RegexFlags::default();
        let mut seen = [false; 256];
        for ch in s.chars() {
            if !ch.is_ascii_alphabetic() {
                return Err(format!("invalid regular expression flag '{}'", ch));
            }
            let b = ch as u8 as usize;
            if seen[b] {
                return Err(format!("duplicate regular expression flag '{}'", ch));
            }
            seen[b] = true;
            match ch {
                'g' => f.global = true,
                'i' => f.ignore_case = true,
                'm' => f.multiline = true,
                's' => f.dot_all = true,
                'y' => f.sticky = true,
                'u' => f.unicode = true,
                _ => return Err(format!("invalid regular expression flag '{}'", ch)),
            }
        }
        Ok(f)
    }

    /// The canonical `gimsuy`-style flag string for `.flags` / display.
    pub fn source(&self) -> String {
        let mut s = String::new();
        if self.global {
            s.push('g');
        }
        if self.ignore_case {
            s.push('i');
        }
        if self.multiline {
            s.push('m');
        }
        if self.dot_all {
            s.push('s');
        }
        if self.unicode {
            s.push('u');
        }
        if self.sticky {
            s.push('y');
        }
        s
    }
}

/// One instruction of the compiled program. `Split(a, b)` tries `a` first
/// (greedy) — the backtracking stack holds the alternative for later.
#[derive(Clone, Debug)]
pub enum Inst {
    Char(char),
    /// Any char — except line terminators unless `s`.
    Any,
    /// `[...]` / `[^...]`. `d`/`w`/`s` are the shorthand members: 1 = the
    /// positive form (`\d`), 0 = the negated form (`\D`), -1 = absent.
    Class {
        negate: bool,
        ranges: Vec<(char, char)>,
        d: i8,
        w: i8,
        s: i8,
    },
    Start,
    End,
    /// Capture slot `i`: even = group start, odd = group end (1-based; 0 is
    /// unused so backreference numbers map 1:1 to slots).
    Save(u16),
    Split(usize, usize),
    Jmp(usize),
    Backref(u16),
    WordBoundary(bool),
    Match,
}

/// A compiled pattern plus its flags and capture count.
pub struct RegexCompiled {
    pub pattern: String,
    pub flags: RegexFlags,
    pub code: Vec<Inst>,
    pub n_captures: usize,
    pub flags_source: String,
}

impl RegexCompiled {
    /// The `.source` form: the pattern with `/` escaped (like V8's source).
    pub fn pattern_source(&self) -> String {
        let mut p = String::new();
        for c in self.pattern.chars() {
            if c == '/' {
                p.push('\\');
            }
            p.push(c);
        }
        p
    }

    /// `String(RegExp)` → `/pattern/flags`.
    pub fn to_source_string(&self) -> String {
        format!("/{}/{}", self.pattern_source(), self.flags.source())
    }
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// JS `\s` — the exact ECMAScript whitespace set (Rust's `is_whitespace`
/// includes NEL `\u{85}`, which JS excludes, so the set is explicit).
fn is_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r'
            | '\u{00A0}' | '\u{1680}' | '\u{2028}' | '\u{2029}'
            | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    ) || ('\u{2000}'..='\u{200A}').contains(&c)
}

pub(crate) fn is_line_term(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn class_member_match(c: char, d: i8, w: i8, s: i8) -> bool {
    (d == 1 && is_digit(c))
        || (d == 0 && !is_digit(c))
        || (w == 1 && is_word(c))
        || (w == 0 && !is_word(c))
        || (s == 1 && is_space(c))
        || (s == 0 && !is_space(c))
}

// ---------------------------------------------------------------- compiler

struct Parser {
    chars: Vec<char>,
    pos: usize,
    code: Vec<Inst>,
    n_captures: usize,
    /// Highest backreference seen, for the validity cap check.
    max_backref: usize,
    /// Total repetition-expansion budget (naive `{n,m}` inlining).
    expand_budget: usize,
}

pub fn compile(pattern: &str, flags: RegexFlags) -> Result<RegexCompiled, String> {
    let mut p = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        code: Vec::new(),
        n_captures: 1,
        max_backref: 0,
        expand_budget: 10_000,
    };
    p.parse_alternation()?;
    if p.pos != p.chars.len() {
        return Err("unexpected ')' in regular expression".to_string());
    }
    if p.max_backref >= p.n_captures {
        return Err("invalid backreference in regular expression".to_string());
    }
    p.code.push(Inst::Match);
    Ok(RegexCompiled {
        pattern: pattern.to_string(),
        flags,
        code: p.code,
        n_captures: p.n_captures,
        flags_source: flags.source(),
    })
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse_alternation(&mut self) -> Result<(), String> {
        let prefix_len = self.code.len();
        let mut starts = Vec::new();
        let s = self.code.len();
        self.parse_sequence()?;
        starts.push((s, self.code.len()));
        while self.peek() == Some('|') {
            self.next();
            let s = self.code.len();
            self.parse_sequence()?;
            starts.push((s, self.code.len()));
        }
        let n = starts.len();
        if n == 1 {
            return Ok(());
        }
        // Restructure into a Split chain: Split(b0, b1); b0; Jmp(end);
        // Split(b1, b2); b1; Jmp(end); ... bn-1; end. The code emitted
        // BEFORE the alternation (a capturing group's opening Save) is
        // preserved as the prefix — rebuilding from scratch would drop it.
        let lens: Vec<usize> = starts.iter().map(|(s, e)| e - s).collect();
        let mut end = prefix_len;
        for i in 0..n - 1 {
            end += 1 + lens[i] + 1;
        }
        end += lens[n - 1];
        let mut new_code = self.code[..prefix_len].to_vec();
        for i in 0..n {
            if i + 1 < n {
                let a = new_code.len() + 1;
                let b = new_code.len() + 1 + lens[i] + 1;
                new_code.push(Inst::Split(a, b));
                new_code.extend_from_slice(&self.code[starts[i].0..starts[i].1]);
                new_code.push(Inst::Jmp(end));
            } else {
                new_code.extend_from_slice(&self.code[starts[i].0..starts[i].1]);
            }
        }
        self.code = new_code;
        Ok(())
    }

    fn parse_sequence(&mut self) -> Result<(), String> {
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            self.parse_atom()?;
        }
        Ok(())
    }

    /// Parse one atom (with its optional quantifier) and emit its code.
    fn parse_atom(&mut self) -> Result<(), String> {
        let atom_start = self.code.len();
        let Some(c) = self.next() else {
            return Err("unexpected end of regular expression".to_string());
        };
        let mut assertion = false;
        match c {
            '^' => {
                self.code.push(Inst::Start);
                assertion = true;
            }
            '$' => {
                self.code.push(Inst::End);
                assertion = true;
            }
            '.' => self.code.push(Inst::Any),
            '(' => self.parse_group()?,
            '[' => self.parse_class()?,
            '\\' => self.parse_escape()?,
            ')' | '|' => return Err("unexpected token in regular expression".to_string()),
            '*' | '+' | '?' => return Err(format!("nothing to repeat at '{}'", c)),
            '{' if self.looks_like_quantifier() => {
                return Err("nothing to repeat at '{'".to_string())
            }
            _ => self.code.push(Inst::Char(c)),
        }
        // Assertions cannot be quantified (`^*`, `\b*` are errors in V8).
        if assertion && self.peek().is_some_and(|c| matches!(c, '*' | '+' | '?' | '{')) {
            return Err("nothing to repeat after assertion".to_string());
        }
        self.parse_quantifier(atom_start)
    }

    /// `{` only counts as a quantifier when it looks like `{n}`, `{n,}` or
    /// `{n,m}` (JS: a lone `{` is a literal). Called with `self.pos` either
    /// at the `{` itself (parse_quantifier) or just past it (parse_atom's
    /// "nothing to repeat" guard), so a leading `{` is skipped either way.
    fn looks_like_quantifier(&self) -> bool {
        let mut i = self.pos;
        let chars = &self.chars;
        if chars.get(i) == Some(&'{') {
            i += 1;
        }
        let mut digits = 0;
        while let Some(d) = chars.get(i) {
            if d.is_ascii_digit() {
                digits += 1;
                i += 1;
            } else {
                break;
            }
        }
        if digits == 0 {
            return false;
        }
        match chars.get(i) {
            Some('}') => true,
            Some(',') => {
                i += 1;
                while let Some(d) = chars.get(i) {
                    if d.is_ascii_digit() {
                        i += 1;
                    } else {
                        break;
                    }
                }
                chars.get(i) == Some(&'}')
            }
            _ => false,
        }
    }

    fn parse_group(&mut self) -> Result<(), String> {
        let capturing = if self.peek() == Some('?') {
            self.next();
            match self.peek() {
                Some(':') => {
                    self.next();
                    false
                }
                Some('=') | Some('!') | Some('<') => {
                    return Err(
                        "lookahead/lookbehind assertions are not supported".to_string(),
                    )
                }
                _ => return Err("invalid group in regular expression".to_string()),
            }
        } else {
            true
        };
        let slot = if capturing {
            let s = self.n_captures;
            self.n_captures += 2;
            self.code.push(Inst::Save(s as u16));
            Some(s)
        } else {
            None
        };
        self.parse_alternation()?;
        if self.next() != Some(')') {
            return Err("unterminated group in regular expression".to_string());
        }
        if let Some(s) = slot {
            self.code.push(Inst::Save((s + 1) as u16));
        }
        Ok(())
    }

    fn parse_class(&mut self) -> Result<(), String> {
        let negate = if self.peek() == Some('^') {
            self.next();
            true
        } else {
            false
        };
        let mut ranges: Vec<(char, char)> = Vec::new();
        let (mut d, mut w, mut s) = (-1i8, -1i8, -1i8);
        let mut first = true;
        loop {
            let Some(c) = self.next() else {
                return Err("unterminated character class in regular expression".to_string());
            };
            if c == ']' && !first {
                break;
            }
            first = false;
            if c == ']' {
                // `[]` (empty class, matches nothing) / `[^]` (matches any
                // char): leave the ranges empty and let the negation flip.
                break;
            }
            if c == '\\' {
                let Some(e) = self.next() else {
                    return Err("unterminated character class in regular expression".to_string());
                };
                match e {
                    'd' => d = 1,
                    'D' => d = 0,
                    'w' => w = 1,
                    'W' => w = 0,
                    's' => s = 1,
                    'S' => s = 0,
                    'b' => ranges.push(('\u{0008}', '\u{0008}')), // \b in class = backspace
                    'n' => ranges.push(('\n', '\n')),
                    't' => ranges.push(('\t', '\t')),
                    'r' => ranges.push(('\r', '\r')),
                    'f' => ranges.push(('\u{000C}', '\u{000C}')),
                    'v' => ranges.push(('\u{000B}', '\u{000B}')),
                    '0' => ranges.push(('\u{0000}', '\u{0000}')),
                    'x' => {
                        let v = self.hex_escape(2)?;
                        ranges.push((v, v))
                    }
                    'u' => {
                        let v = self.hex_escape(4)?;
                        ranges.push((v, v))
                    }
                    '-' => ranges.push(('-', '-')),
                    ']' => ranges.push((']', ']')),
                    '^' => ranges.push(('^', '^')),
                    '\\' => ranges.push(('\\', '\\')),
                    other => ranges.push((other, other)),
                }
            } else if c == '-' && self.peek() == Some(']') {
                ranges.push(('-', '-'));
            } else {
                // Range `a-z`?
                if self.peek() == Some('-')
                    && self.chars.get(self.pos + 1).is_some_and(|n| *n != ']')
                {
                    self.next(); // -
                    let Some(hi) = self.next() else {
                        return Err(
                            "unterminated character class in regular expression".to_string()
                        );
                    };
                    if hi == '\\' {
                        // Range end escapes like `[a-\d]` are invalid in JS.
                        return Err("invalid character class range".to_string());
                    }
                    if hi < c {
                        return Err("range out of order in character class".to_string());
                    }
                    ranges.push((c, hi));
                } else {
                    ranges.push((c, c));
                }
            }
        }
        self.code
            .push(Inst::Class { negate, ranges, d, w, s });
        Ok(())
    }

    fn hex_escape(&mut self, n: usize) -> Result<char, String> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let Some(c) = self.next() else {
                return Err("invalid hex escape in regular expression".to_string());
            };
            let d = c
                .to_digit(16)
                .ok_or("invalid hex escape in regular expression")?;
            v = v * 16 + d;
        }
        char::from_u32(v).ok_or_else(|| "invalid code point in regular expression".to_string())
    }

    fn parse_escape(&mut self) -> Result<(), String> {
        let Some(c) = self.next() else {
            return Err("trailing backslash in regular expression".to_string());
        };
        let inst = match c {
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                let (d, w, s) = match c {
                    'd' => (1, -1, -1),
                    'D' => (0, -1, -1),
                    'w' => (-1, 1, -1),
                    'W' => (-1, 0, -1),
                    's' => (-1, -1, 1),
                    _ => (-1, -1, 0),
                };
                Inst::Class {
                    negate: false,
                    ranges: Vec::new(),
                    d,
                    w,
                    s,
                }
            }
            'b' => Inst::WordBoundary(true),
            'B' => Inst::WordBoundary(false),
            'n' => Inst::Char('\n'),
            't' => Inst::Char('\t'),
            'r' => Inst::Char('\r'),
            'f' => Inst::Char('\u{000C}'),
            'v' => Inst::Char('\u{000B}'),
            '0' => {
                if self.peek().is_some_and(|n| n.is_ascii_digit()) {
                    return Err("decimal escape not allowed in regular expression".to_string());
                }
                Inst::Char('\u{0000}')
            }
            'x' => Inst::Char(self.hex_escape(2)?),
            'u' => Inst::Char(self.hex_escape(4)?),
            'c' => {
                let Some(ctl) = self.next() else {
                    return Err("invalid control escape in regular expression".to_string());
                };
                if ctl.is_ascii_alphabetic() {
                    Inst::Char(((ctl as u8) % 32) as char)
                } else {
                    return Err("invalid control escape in regular expression".to_string());
                }
            }
            '1'..='9' => {
                let n = (c as u8 - b'0') as usize;
                self.max_backref = self.max_backref.max(n);
                Inst::Backref(n as u16)
            }
            other => Inst::Char(other), // escaped punctuation, `/`, etc.
        };
        self.code.push(inst);
        Ok(())
    }

    fn parse_quantifier(&mut self, atom_start: usize) -> Result<(), String> {
        let Some(c) = self.peek() else { return Ok(()) };
        let (min, max): (usize, Option<usize>) = match c {
            '*' => {
                self.next();
                (0, None)
            }
            '+' => {
                self.next();
                (1, None)
            }
            '?' => {
                self.next();
                (0, Some(1))
            }
            '{' if self.looks_like_quantifier() => {
                self.next();
                let mut lo = 0usize;
                while self.peek().is_some_and(|d| d.is_ascii_digit()) {
                    lo = lo
                        .saturating_mul(10)
                        .saturating_add(self.next().unwrap() as u8 as usize - b'0' as usize);
                }
                let hi = if self.peek() == Some(',') {
                    self.next();
                    if self.peek() == Some('}') {
                        None
                    } else {
                        let mut hi = 0usize;
                        while self.peek().is_some_and(|d| d.is_ascii_digit()) {
                            hi = hi.saturating_mul(10).saturating_add(
                                self.next().unwrap() as u8 as usize - b'0' as usize,
                            );
                        }
                        Some(hi)
                    }
                } else {
                    Some(lo)
                };
                if self.next() != Some('}') {
                    return Err("invalid quantifier in regular expression".to_string());
                }
                if let Some(hi) = hi {
                    if hi < lo {
                        return Err("numbers out of order in {} quantifier".to_string());
                    }
                    if lo > 1000 || hi > 1000 {
                        return Err("regular expression quantifier too large".to_string());
                    }
                } else if lo > 1000 {
                    return Err("regular expression quantifier too large".to_string());
                }
                (lo, hi)
            }
            _ => return Ok(()),
        };
        let lazy = self.peek() == Some('?');
        if lazy {
            self.next();
        }
        let atom_len = self.code.len() - atom_start;
        self.apply_quantifier(atom_start, atom_len, min, max, lazy)?;
        Ok(())
    }

    fn apply_quantifier(
        &mut self,
        atom_start: usize,
        atom_len: usize,
        min: usize,
        max: Option<usize>,
        lazy: bool,
    ) -> Result<(), String> {
        let total = max.map(|m| m.saturating_add(min)).unwrap_or(min);
        if total > self.expand_budget {
            return Err("regular expression too complex".to_string());
        }
        // Snapshot the atom and re-inline it (capture slots are shared, so
        // the last repetition wins — JS semantics).
        let atom: Vec<Inst> = self.code[atom_start..atom_start + atom_len].to_vec();
        self.code.truncate(atom_start);

        for _ in 0..min {
            self.code.extend_from_slice(&atom);
        }
        match max {
            Some(hi) if hi > min => {
                // Optional repetitions: each is Split(try, skip); atom.
                for _ in min..hi {
                    let split_at = self.code.len();
                    self.code.push(Inst::Split(0, 0));
                    let a = self.code.len();
                    self.code.extend_from_slice(&atom);
                    let b = self.code.len();
                    let (first, second) = if lazy { (b, a) } else { (a, b) };
                    self.code[split_at] = Inst::Split(first, second);
                }
            }
            Some(_) => {}
            None => {
                // Unbounded greedy: Split(body, out); atom; Jmp(split). The
                // exit sits AFTER the Jmp, and the Jmp must return to the
                // SPLIT (not the body) so each iteration re-pushes the exit
                // with the current position — otherwise the exit keeps its
                // first-iteration position and `a+` on "aaa" matches 1 char.
                let split_at = self.code.len();
                self.code.push(Inst::Split(0, 0));
                let a = self.code.len();
                self.code.extend_from_slice(&atom);
                self.code.push(Inst::Jmp(split_at));
                let out = self.code.len();
                let (first, second) = if lazy { (out, a) } else { (a, out) };
                self.code[split_at] = Inst::Split(first, second);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- matcher

struct Thread {
    pc: usize,
    pos: usize,
    /// Raw capture slots: slot 2g-1 = group g start, slot 2g = group g end
    /// (each `Some((pos, pos))`). Group ranges are reconstructed on return.
    caps: Vec<Option<(usize, usize)>>,
}

/// Reconstruct per-group `(start, end)` ranges from the raw slots. A group
/// participates only when BOTH its slots were written (entered AND exited).
fn group_ranges(
    slots: &[Option<(usize, usize)>],
    n_captures: usize,
) -> Vec<Option<(usize, usize)>> {
    let mut out = vec![None; n_captures];
    // n_captures = 1 + 2G (slot 0 unused); group g uses slots 2g-1, 2g.
    for g in 1..=n_captures / 2 {
        if let (Some(s), Some(e)) = (slots[2 * g - 1], slots[2 * g]) {
            out[g] = Some((s.0, e.1));
        }
    }
    out
}

/// Run the program at `start`; on success return (end pos, group ranges).
fn match_at(
    prog: &RegexCompiled,
    chars: &[char],
    start: usize,
) -> Result<Option<(usize, Vec<Option<(usize, usize)>>)>, RegexExecError> {
    let n = prog.n_captures;
    let mut stack: Vec<Thread> = Vec::new();
    stack.push(Thread {
        pc: 0,
        pos: start,
        caps: vec![None; n],
    });
    // Catastrophic backtracking guard: bound steps to prevent exponential blowup on patterns like (a+)+b
    let mut steps: usize = 0;
    const MAX_STEPS: usize = 1_000_000;
    while let Some(Thread { pc, pos, caps }) = stack.pop() {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(RegexExecError::TooComplex);
        }
        let Some(inst) = prog.code.get(pc) else { continue };
        let ic = prog.flags.ignore_case;
        match inst {
            Inst::Char(c) => {
                if let Some(&cc) = chars.get(pos) {
                    if char_eq(*c, cc, ic) {
                        stack.push(Thread {
                            pc: pc + 1,
                            pos: pos + 1,
                            caps,
                        });
                    }
                }
            }
            Inst::Any => {
                if let Some(&cc) = chars.get(pos) {
                    if prog.flags.dot_all || !is_line_term(cc) {
                        stack.push(Thread {
                            pc: pc + 1,
                            pos: pos + 1,
                            caps,
                        });
                    }
                }
            }
            Inst::Class {
                negate,
                ranges,
                d,
                w,
                s,
            } => {
                if let Some(&cc) = chars.get(pos) {
                    // With /i, also try the ASCII-folded variant so `[aeiou]`
                    // matches "HELLO" like Node (the folding is ASCII-only,
                    // a documented limit).
                    let mut matched = false;
                    if ic {
                        let fold = if cc.is_ascii_lowercase() {
                            cc.to_ascii_uppercase()
                        } else {
                            cc.to_ascii_lowercase()
                        };
                        for cand in [cc, fold] {
                            let mut m = class_member_match(cand, *d, *w, *s);
                            if !m {
                                for (lo, hi) in ranges {
                                    if cand >= *lo && cand <= *hi {
                                        m = true;
                                        break;
                                    }
                                }
                            }
                            if m != *negate {
                                matched = true;
                                break;
                            }
                        }
                    } else {
                        let mut m = class_member_match(cc, *d, *w, *s);
                        if !m {
                            for (lo, hi) in ranges {
                                if cc >= *lo && cc <= *hi {
                                    m = true;
                                    break;
                                }
                            }
                        }
                        matched = m != *negate;
                    }
                    if matched {
                        stack.push(Thread {
                            pc: pc + 1,
                            pos: pos + 1,
                            caps,
                        });
                    }
                }
            }
            Inst::Start => {
                let ok = pos == 0
                    || (prog.flags.multiline && pos > 0 && is_line_term(chars[pos - 1]));
                if ok {
                    stack.push(Thread { pc: pc + 1, pos, caps });
                }
            }
            Inst::End => {
                let ok = pos == chars.len() || (prog.flags.multiline && is_line_term(chars[pos]));
                if ok {
                    stack.push(Thread { pc: pc + 1, pos, caps });
                }
            }
            Inst::Save(i) => {
                let mut c2 = caps;
                if let Some(slot) = c2.get_mut(*i as usize) {
                    *slot = Some((pos, pos));
                }
                stack.push(Thread {
                    pc: pc + 1,
                    pos,
                    caps: c2,
                });
            }
            Inst::Split(a, b) => {
                stack.push(Thread {
                    pc: *b,
                    pos,
                    caps: caps.clone(),
                });
                stack.push(Thread { pc: *a, pos, caps });
            }
            Inst::Jmp(t) => stack.push(Thread { pc: *t, pos, caps }),
            Inst::Backref(i) => {
                // Group g lives in slots 2g-1 (start) and 2g (end). A group
                // that did not participate matches empty (ES spec).
                let g = *i as usize;
                match (caps.get(2 * g - 1).and_then(|c| *c), caps.get(2 * g).and_then(|c| *c)) {
                    (Some(s), Some(e)) => {
                        let len = e.1.saturating_sub(s.0);
                        if pos + len <= chars.len() && chars[pos..pos + len] == chars[s.0..s.0 + len] {
                            stack.push(Thread {
                                pc: pc + 1,
                                pos: pos + len,
                                caps,
                            });
                        }
                    }
                    _ => stack.push(Thread { pc: pc + 1, pos, caps }),
                }
            }
            Inst::WordBoundary(b) => {
                let before = pos > 0 && is_word(chars[pos - 1]);
                let after = pos < chars.len() && is_word(chars[pos]);
                if (before != after) == *b {
                    stack.push(Thread { pc: pc + 1, pos, caps });
                }
            }
            Inst::Match => {
                return Ok(Some((pos, group_ranges(&caps, prog.n_captures))));
            }
        }
    }
    Ok(None)
}

fn char_eq(a: char, b: char, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegexExecError {
    TooComplex,
}

impl std::fmt::Display for RegexExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegexExecError::TooComplex => write!(f, "regular expression too complex"),
        }
    }
}

impl std::error::Error for RegexExecError {}

/// Result of one exec/search: (start, end, captures) in char positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub caps: Vec<Option<(usize, usize)>>,
}

/// Find the first match at or after `start` (char index). When `sticky` is
/// set, only `start` itself is tried.
pub fn search(prog: &RegexCompiled, chars: &[char], start: usize) -> Result<Option<Match>, RegexExecError> {
    let start = start.min(chars.len());
    if prog.flags.sticky {
        return match_at(prog, chars, start).map(|opt| {
            opt.map(|(end, caps)| Match {
                start,
                end,
                caps,
            })
        });
    }
    for s in start..=chars.len() {
        if let Some((end, caps)) = match_at(prog, chars, s)? {
            return Ok(Some(Match { start: s, end, caps }));
        }
    }
    Ok(None)
}

/// Convert a char position to a UTF-16 code-unit offset (V8's `.index` /
/// `lastIndex` unit), so surrogate pairs count double like in Node.
pub fn char_pos_to_utf16(chars: &[char], pos: usize) -> usize {
    chars[..pos.min(chars.len())]
        .iter()
        .map(|c| c.len_utf16())
        .sum()
}

/// All non-overlapping matches for a global scan (`String.match` with /g,
/// `String.replace` with /g, `String.split`). Empty matches advance by one
/// char, matching ES `RegExpExec`'s empty-match advance rule.
pub fn scan_all(
    prog: &RegexCompiled,
    chars: &[char],
) -> Result<Vec<(usize, usize, Vec<Option<(usize, usize)>>)>, RegexExecError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos <= chars.len() {
        match search(prog, chars, pos)? {
            Some(m) => {
                out.push((m.start, m.end, m.caps));
                if m.end > m.start {
                    pos = m.end;
                } else {
                    pos = m.start + 1;
                }
            }
            None => break,
        }
    }
    Ok(out)
}

/// Convenience for the VM: build a compiled regex from pattern+flags text,
/// returning a human-readable error on failure (used by MakeRegex at runtime
/// for hand-crafted bytecode; source compiles validate at lex time).
pub fn compile_from_str(pattern: &str, flags_str: &str) -> Result<RegexCompiled, String> {
    let flags = RegexFlags::parse(flags_str)?;
    compile(pattern, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, hay: &str) -> Option<(usize, usize)> {
        let prog = compile_from_str(pattern, "").expect("compile");
        let chars: Vec<char> = hay.chars().collect();
        search(&prog, &chars, 0).unwrap().map(|m| (m.start, m.end))
    }

    fn caps(pattern: &str, hay: &str) -> Vec<Option<(usize, usize)>> {
        let prog = compile_from_str(pattern, "").expect("compile");
        let chars: Vec<char> = hay.chars().collect();
        search(&prog, &chars, 0).unwrap().map(|m| m.caps).unwrap()
    }

    #[test]
    fn literals_and_anchors() {
        assert_eq!(m("abc", "xxabc"), Some((2, 5)));
        assert_eq!(m("^abc", "xxabc"), None);
        assert_eq!(m("^abc", "abc"), Some((0, 3)));
        assert_eq!(m("abc$", "abc"), Some((0, 3)));
        assert_eq!(m("abc$", "abcx"), None);
        assert_eq!(m("a.c", "axc"), Some((0, 3)));
        assert_eq!(m("a.c", "a\nc"), None);
        assert_eq!(m("", "x"), Some((0, 0)));
    }

    #[test]
    fn classes() {
        assert_eq!(m("[a-z]+", "123abc456"), Some((3, 6)));
        assert_eq!(m("[^0-9]+", "123abc456"), Some((3, 6)));
        assert_eq!(m("\\d+", "ab12cd"), Some((2, 4)));
        assert_eq!(m("\\w+", "a b"), Some((0, 1)));
        assert_eq!(m("\\s+", "a \t b"), Some((1, 4)));
        assert_eq!(m("[a\\-z]+", "--"), Some((0, 2)));
        assert_eq!(m("[]", "x"), None);
        assert_eq!(m("[^]", "x"), Some((0, 1)));
    }


    #[test]
    fn quantifiers() {
        assert_eq!(m("a*", "bbb"), Some((0, 0)));
        assert_eq!(m("a+", "bbb"), None);
        assert_eq!(m("a?b", "bbb"), Some((0, 1))); // optional a skipped, b at 0
        assert_eq!(m("a{2}", "aaa"), Some((0, 2)));
        assert_eq!(m("a{2,}", "aaaa"), Some((0, 4)));
        assert_eq!(m("a{1,3}", "aaaa"), Some((0, 3)));
        assert_eq!(m("a{1,3}b", "aab"), Some((0, 3)));
        // Greedy vs lazy.
        assert_eq!(m("<.+>", "<a><b>"), Some((0, 6)));
        assert_eq!(m("<.+?>", "<a><b>"), Some((0, 3)));
    }

    #[test]
    fn groups_and_alternation() {
        assert_eq!(m("(ab|cd)+", "cdab"), Some((0, 4)));
        assert_eq!(m("a|b", "z"), None);
        assert_eq!(m("a|", "z"), Some((0, 0)));
        assert_eq!(m("(?:ab)+", "abab"), Some((0, 4)));
        let c = caps("(a)(b)?", "a");
        assert_eq!(c[1], Some((0, 1)));
        assert_eq!(c[2], None); // non-participating group → undefined
        let c = caps("(a)(b)", "ab");
        assert_eq!(c[1], Some((0, 1)));
        assert_eq!(c[2], Some((1, 2)));
    }

    #[test]
    fn backrefs_and_boundaries() {
        assert_eq!(m("(ab)\\1", "abab"), Some((0, 4)));
        assert_eq!(m("(ab)\\1", "ababab"), Some((0, 4)));
        assert_eq!(m("\\bword\\b", "a word!"), Some((2, 6)));
        assert_eq!(m("\\B", "aa"), Some((1, 1)));
    }

    #[test]
    fn flags() {
        let p = compile_from_str("abc", "i").unwrap();
        let chars: Vec<char> = "ABC".chars().collect();
        assert!(search(&p, &chars, 0).unwrap().is_some());
        let p = compile_from_str("^a", "m").unwrap();
        let chars: Vec<char> = "x\na".chars().collect();
        assert_eq!(search(&p, &chars, 0).unwrap().map(|m| m.start), Some(2));
        let p = compile_from_str("a.c", "s").unwrap();
        let chars: Vec<char> = "a\nc".chars().collect();
        assert!(search(&p, &chars, 0).unwrap().is_some());
        let p = compile_from_str("b", "y").unwrap();
        let chars: Vec<char> = "abc".chars().collect();
        assert!(search(&p, &chars, 0).unwrap().is_none());
        assert!(search(&p, &chars, 1).unwrap().is_some());
    }

    #[test]
    fn scan_and_errors() {
        let p = compile_from_str("\\d+", "g").unwrap();
        let chars: Vec<char> = "a12b34".chars().collect();
        let all = scan_all(&p, &chars).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].0, all[0].1), (1, 3));
        assert_eq!((all[1].0, all[1].1), (4, 6));
        assert!(compile_from_str("(ab", "").is_err());
        assert!(compile_from_str("a{2,1}", "").is_err());
        assert!(compile_from_str("a**", "").is_err());
        assert!(compile_from_str("*a", "").is_err());
        assert!(compile_from_str("\\1", "").is_err());
        assert!(compile_from_str("[a-", "").is_err());
        assert!(compile_from_str("a", "zz").is_err());
        assert!(compile_from_str("a", "gg").is_err());
        assert!(compile_from_str("(?=a)", "").is_err());
    }

    #[test]
    fn backtracking_guard() {
        let p = compile_from_str("(a+)+b", "").unwrap();
        let hay: Vec<char> = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".chars().collect();
        let res = search(&p, &hay, 0);
        assert_eq!(res, Err(RegexExecError::TooComplex));
    }
}
