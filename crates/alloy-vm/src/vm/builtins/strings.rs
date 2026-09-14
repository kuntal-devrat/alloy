use alloy_core::regex::{self, RegexCompiled};
use alloy_core::value::{to_string_js, RegexState, Value, VmHost};
use std::sync::{Arc, Mutex};

/// JS `String.prototype.trim` whitespace: the WhiteSpace + LineTerminator set
/// (`\u0009-\u000D`, `\u0020`, `\u00A0`, `\u1680`, `\u2000-\u200A`,
/// `\u2028`, `\u2029`, `\u202F`, `\u205F`, `\u3000`, `\uFEFF`). Rust's
/// `is_whitespace` covers every entry except `\uFEFF` (the BOM), which JS
/// trims — so strip it explicitly.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// First index of `needle` in `hay` at or after `from` (char positions, like
/// JS indexOf operates on code units — for the ASCII corpus both agree). An
/// empty needle matches at `min(from, len)`. `None` when absent.
pub(crate) fn char_index_of(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    let n = hay.len();
    let m = needle.len();
    if m == 0 {
        return Some(from.min(n));
    }
    if from > n || m > n - from {
        return None;
    }
    let mut i = from;
    while i <= n - m {
        if hay[i..i + m] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Last index of `needle` in `hay` at or before `start` (char positions). An
/// empty needle matches at `min(start, len)`. `None` when absent.
pub(crate) fn char_last_index_of(hay: &[char], needle: &[char], start: usize) -> Option<usize> {
    let n = hay.len();
    let m = needle.len();
    if m == 0 {
        return Some(start.min(n));
    }
    if m > n {
        return None;
    }
    let mut i = start.min(n - m);
    loop {
        if hay[i..i + m] == *needle {
            return Some(i);
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

/// Expand JS `$` patterns in a replacement string: `$$` → `$`, `$&` → the
/// matched text, `$`` → text before the match, `$'` → text after the match.
/// `$n` digits are kept literally (no capture groups without regex).
pub(crate) fn expand_replacement(
    repl: &str,
    matched: &str,
    before: &str,
    after: &str,
    caps: &[Option<(usize, usize)>],
    hay: &[char],
) -> String {
    let mut out = String::with_capacity(repl.len() + matched.len());
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let peeked = chars.peek().copied();
            match peeked {
                Some('$') => {
                    out.push('$');
                    chars.next();
                }
                Some('&') => {
                    out.push_str(matched);
                    chars.next();
                }
                Some('`') => {
                    out.push_str(before);
                    chars.next();
                }
                Some('\'') => {
                    out.push_str(after);
                    chars.next();
                }
                Some(d) if d.is_ascii_digit() => {
                    // `$1`..`$99` capture references — the capture text, or
                    // "" when the group didn't participate or is out of
                    // range. A two-digit `$nn` is used only when `nn` names a
                    // real capture (ES spec); otherwise the single digit.
                    let n_groups = (caps.len().saturating_sub(1)) / 2;
                    let mut num = (d as u8 - b'0') as usize;
                    let mut consumed = 1;
                    let nxt = chars.peek().copied();
                    if let Some(d2) = nxt {
                        if d2.is_ascii_digit() {
                            let two = num * 10 + (d2 as u8 - b'0') as usize;
                            if two <= n_groups {
                                num = two;
                                consumed = 2;
                            }
                        }
                    }
                    let text = if num <= n_groups {
                        match caps.get(num).and_then(|c| *c) {
                            Some((x, y)) => hay.get(x..y).map(|c| c.iter().collect::<String>()),
                            None => None,
                        }
                    } else {
                        None
                    };
                    match text {
                        Some(t) => out.push_str(&t),
                        None => {
                            out.push('$');
                            out.push(d);
                        }
                    }
                    for _ in 0..consumed {
                        chars.next();
                    }
                }
                _ => out.push('$'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Regular expressions — exec/test/match/replace/search/split runtime
// ---------------------------------------------------------------------------

/// Convert a UTF-16 code-unit offset (V8's `lastIndex` unit) back to a char
/// index. Surrogate pairs count double, so a `lastIndex` set mid-pair lands
/// on the pair's start char.
pub(crate) fn utf16_to_char(chars: &[char], u16pos: usize) -> usize {
    let mut acc = 0usize;
    for (i, c) in chars.iter().enumerate() {
        if acc >= u16pos {
            return i;
        }
        acc += c.len_utf16();
    }
    chars.len()
}

/// One exec/test attempt: run the regex against `arg` (coerced via ToString)
/// at `lastIndex` (only honored for /g /y, like Node), advancing `lastIndex`
/// on success (empty matches advance one code unit so /g terminates) and
/// resetting it to 0 when a /g search fails.
pub(crate) struct RegexExecOutcome {
    matched: Result<Option<regex::Match>, regex::RegexExecError>,
    hay: Vec<char>,
    hay_text: String,
}

pub(crate) fn regex_exec_core(st: &Arc<Mutex<RegexState>>, arg: &Value) -> RegexExecOutcome {
    let mut g = st.lock().unwrap_or_else(|g| g.into_inner());
    let hay_text = to_string_js(arg);
    let hay: Vec<char> = hay_text.chars().collect();
    let flags = g.compiled.flags;
    let use_last = flags.global || flags.sticky;
    let start_char = if use_last {
        utf16_to_char(&hay, g.last_index)
    } else {
        0
    };
    let matched = regex::search(&g.compiled, &hay, start_char);
    match &matched {
        Ok(None) => {
            if flags.global {
                g.last_index = 0;
            }
        }
        Ok(Some(m)) => {
            if use_last {
                g.last_index = if m.end == m.start {
                    regex::char_pos_to_utf16(&hay, m.start) + 1
                } else {
                    regex::char_pos_to_utf16(&hay, m.end)
                };
            }
        }
        Err(_) => {}
    }
    RegexExecOutcome {
        matched,
        hay,
        hay_text,
    }
}

/// Build the exec result: an object with `0` = full match, `1..G` = capture
/// texts (undefined when a group didn't participate), plus `length`,
/// `index` (UTF-16), and `input`. Object-shaped rather than array-shaped
/// (the engine has no getter-backed array subtypes) — element reads, length,
/// index and input all work; spread/`Array.isArray` do not (documented).
pub(crate) fn regex_exec_value(
    st: &Arc<Mutex<RegexState>>,
    arg: &Value,
    vm: &mut dyn VmHost,
) -> Value {
    let out = regex_exec_core(st, arg);
    let m = match out.matched {
        Ok(Some(m)) => m,
        Ok(None) => return Value::null(),
        Err(e) => {
            vm.throw_exception(Value::string(format!(
                "SyntaxError: Invalid regular expression: {}",
                e
            )));
            return Value::null();
        }
    };
    let hay = &out.hay;
    let n_groups = (m.caps.len().saturating_sub(1)) / 2;
    let mut entries: Vec<(String, Value)> = Vec::with_capacity(n_groups + 4);
    let full: String = hay[m.start..m.end].iter().collect();
    entries.push(("0".to_string(), Value::string(full)));
    for g in 1..=n_groups {
        let v = match m.caps.get(g).and_then(|c| *c) {
            Some((x, y)) => Value::string(hay[x..y].iter().collect()),
            None => Value::undefined(),
        };
        entries.push((g.to_string(), v));
    }
    entries.push(("length".to_string(), Value::int(n_groups as i64 + 1)));
    entries.push((
        "index".to_string(),
        Value::int(regex::char_pos_to_utf16(hay, m.start) as i64),
    ));
    entries.push(("input".to_string(), Value::string(out.hay_text)));
    Value::object_ordered(entries)
}

/// The regex value's own property surface: read-only flags, `lastIndex`
/// (mutable), and the exec/test/toString methods.
pub(crate) fn regex_prop(obj: &Value, name: &str) -> Value {
    let st = obj
        .as_regex()
        .expect("regex_prop called on a regex")
        .clone();
    let static_val = {
        let g = st.lock().unwrap_or_else(|g| g.into_inner());
        match name {
            "source" => Some(Value::string(g.compiled.pattern_source())),
            "flags" => Some(Value::string(g.compiled.flags.source())),
            "global" => Some(Value::bool(g.compiled.flags.global)),
            "ignoreCase" => Some(Value::bool(g.compiled.flags.ignore_case)),
            "multiline" => Some(Value::bool(g.compiled.flags.multiline)),
            "dotAll" => Some(Value::bool(g.compiled.flags.dot_all)),
            "sticky" => Some(Value::bool(g.compiled.flags.sticky)),
            "unicode" => Some(Value::bool(g.compiled.flags.unicode)),
            "lastIndex" => Some(Value::int(g.last_index as i64)),
            _ => None,
        }
    };
    if let Some(v) = static_val {
        return v;
    }
    match name {
        "exec" => Value::native(Arc::new(move |args, vm| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            regex_exec_value(&st, &arg, vm)
        })),
        "test" => Value::native(Arc::new(move |args, vm| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            match regex_exec_core(&st, &arg).matched {
                Ok(m) => Value::bool(m.is_some()),
                Err(e) => {
                    vm.throw_exception(Value::string(format!(
                        "SyntaxError: Invalid regular expression: {}",
                        e
                    )));
                    Value::bool(false)
                }
            }
        })),
        "toString" => Value::native(Arc::new(move |_args, _vm| {
            let g = st.lock().unwrap_or_else(|g| g.into_inner());
            Value::string(g.compiled.to_source_string())
        })),
        _ => Value::undefined(),
    }
}
/// `String.prototype` method reads for `s.name` — natives capturing the
/// string. Semantics follow JS: `charAt` returns the char at the index ("" out
/// of bounds; negative counts from the end), `substring` clamps negatives to
/// zero and swaps inverted bounds, `split` on "" splits into characters, on an
/// undefined separator returns `[s]`, `toUpperCase` maps through Unicode
/// uppercase (ASCII-exact for the engine's corpus).
pub(crate) fn string_prop(obj: &Value, name: &str) -> Value {
    let s = obj.clone();
    match name {
        // JS length counts UTF-16 code units ("😀".length is 2), not code points.
        "length" => Value::int(
            s.as_str()
                .map(|s| s.encode_utf16().count() as i64)
                .unwrap_or(0),
        ),
        "charAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            // JS charAt: NaN → 0, and anything out of [0, len) is "" —
            // negatives do NOT count from the end (that's `at`).
            if i < 0 || i >= n {
                Value::string(String::new())
            } else {
                Value::string(chars[i as usize].to_string())
            }
        })),
        "substring" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let n = s.chars().count() as i64;
            let clamp = |v: Option<&Value>, dflt: i64| -> i64 {
                match v {
                    Some(v) if v.is_undefined() => dflt,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else {
                            x.trunc().clamp(0.0, n as f64) as i64
                        }
                    }
                    None => dflt,
                }
            };
            let mut a = clamp(args.first(), 0);
            let mut b = clamp(args.get(1), n);
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let out: String = s.chars().skip(a as usize).take((b - a) as usize).collect();
            Value::string(out)
        })),
        "split" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            // Regex separator: split on every match, including participating
            // capture groups in the result (JS semantics), empty matches
            // split between chars.
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                // Empty input: an empty-matching regex yields [] (Node), a
                // non-matching one [""] (the single empty part).
                if hay.is_empty() {
                    let matched = match regex::search(&prog, &hay, 0) {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(e) => {
                            vm.throw_exception(Value::string(format!(
                                "SyntaxError: Invalid regular expression: {}",
                                e
                            )));
                            return Value::undefined();
                        }
                    };
                    return Value::array(if matched {
                        Vec::new()
                    } else {
                        vec![Value::string(String::new())]
                    });
                }
                let mut parts: Vec<Value> = Vec::new();
                let mut pos = 0usize;
                let mut terminal_empty = false;
                let scan_res = match regex::scan_all(&prog, &hay) {
                    Ok(ms) => ms,
                    Err(e) => {
                        vm.throw_exception(Value::string(format!(
                            "SyntaxError: Invalid regular expression: {}",
                            e
                        )));
                        return Value::undefined();
                    }
                };
                for (a, b, caps) in scan_res {
                    if b > a {
                        // Non-empty separator: always push the pre-segment
                        // (even "" when the separator sits at position 0,
                        // like `"ab".split(/(a)/)` -> ["", "a", "b"]).
                        parts.push(Value::string(hay[pos..a].iter().collect()));
                        let n_groups = (caps.len().saturating_sub(1)) / 2;
                        for g in 1..=n_groups {
                            if let Some((x, y)) = caps.get(g).and_then(|c| *c) {
                                parts.push(Value::string(hay[x..y].iter().collect()));
                            }
                        }
                        pos = b;
                    } else if a < hay.len() {
                        // Empty separator between chars: the char itself
                        // becomes the part ("abc".split(/(?:)/) -> ["a",
                        // "b", "c"]).
                        parts.push(Value::string(hay[a..a + 1].iter().collect()));
                        pos = a + 1;
                    } else {
                        // Terminal empty match: nothing after it, and no
                        // trailing empty part (Node: "abc".split(/(?:)/)
                        // ends at "c").
                        terminal_empty = true;
                        break;
                    }
                }
                if !terminal_empty {
                    parts.push(Value::string(hay[pos..].iter().collect()));
                }
                return Value::array(parts);
            }
            let parts: Vec<Value> = match args.first() {
                None => vec![Value::string(s.to_string())],
                Some(v) if v.is_undefined() => vec![Value::string(s.to_string())],
                Some(v) if v.as_str() == Some("") => {
                    s.chars().map(|c| Value::string(c.to_string())).collect()
                }
                Some(v) => match v.as_str() {
                    Some(sep) if sep.is_empty() => {
                        s.chars().map(|c| Value::string(c.to_string())).collect()
                    }
                    Some(sep) => s.split(sep).map(|p| Value::string(p.to_string())).collect(),
                    // Non-string separator: coerce like JS ToString.
                    None => s
                        .split(&to_string_js(v))
                        .map(|p| Value::string(p.to_string()))
                        .collect(),
                },
            };
            Value::array(parts)
        })),
        "toUpperCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_uppercase())
        })),
        "trim" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(js_trim(s.as_str().unwrap_or("")).to_string())
        })),
        "slice" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let arg = |i: usize, dflt: i64| -> i64 {
                match args.get(i) {
                    Some(v) if v.is_undefined() => dflt,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else if x.is_infinite() {
                            if x > 0.0 {
                                n
                            } else {
                                0
                            }
                        } else {
                            x.trunc() as i64
                        }
                    }
                    None => dflt,
                }
            };
            let mut a = arg(0, 0);
            let mut b = arg(1, n);
            if a < 0 {
                a = (n + a).max(0);
            }
            if b < 0 {
                b = (n + b).max(0);
            }
            a = a.min(n);
            b = b.min(n);
            if a > b {
                Value::string(String::new())
            } else {
                let out: String = chars[a as usize..b as usize].iter().collect();
                Value::string(out)
            }
        })),
        "substr" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let chars: Vec<char> = s.chars().collect();
            let n = chars.len() as i64;
            let start = match args.first() {
                Some(v) if v.is_undefined() => 0i64,
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else if x.is_infinite() {
                        if x > 0.0 {
                            n
                        } else {
                            0
                        }
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let start = if start < 0 {
                (n + start).max(0)
            } else {
                start.min(n)
            };
            let mut len = match args.get(1) {
                Some(v) if v.is_undefined() => n - start,
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else if x.is_infinite() {
                        if x > 0.0 {
                            n - start
                        } else {
                            return Value::string(String::new());
                        }
                    } else {
                        x.trunc() as i64
                    }
                }
                None => n - start,
            };
            if len <= 0 {
                return Value::string(String::new());
            }
            len = len.min(n - start);
            let out: String = chars[start as usize..(start + len) as usize]
                .iter()
                .collect();
            Value::string(out)
        })),
        "includes" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let pos = if num_pos.is_nan() {
                0.0
            } else {
                num_pos.trunc()
            };
            // +∞ fromIndex → false (ES); empty search otherwise always true.
            if pos.is_infinite() && pos > 0.0 {
                return Value::bool(false);
            }
            let start = (pos.max(0.0) as i64).min(hay.len() as i64) as usize;
            if needle.is_empty() {
                return Value::bool(true);
            }
            Value::bool(char_index_of(&hay, &needle, start).is_some())
        })),
        "startsWith" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let pos = if num_pos.is_nan() {
                0.0
            } else {
                num_pos.trunc()
            };
            let start = if pos.is_infinite() {
                if pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (pos.max(0.0) as i64).min(hay.len() as i64) as usize
            };
            if needle.is_empty() {
                return Value::bool(true);
            }
            if start + needle.len() > hay.len() {
                return Value::bool(false);
            }
            Value::bool(hay[start..start + needle.len()] == needle[..])
        })),
        "endsWith" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::INFINITY,
                Some(v) => v.to_number(),
                None => f64::INFINITY,
            };
            let pos = if num_pos.is_nan() {
                // Explicit NaN → 0 (ToIntegerOrInfinity); undefined → +∞.
                0.0
            } else {
                num_pos.trunc()
            };
            let end = if pos.is_infinite() {
                if pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (pos.max(0.0) as i64).min(hay.len() as i64) as usize
            };
            if needle.is_empty() {
                return Value::bool(true);
            }
            if needle.len() > end {
                return Value::bool(false);
            }
            Value::bool(hay[end - needle.len()..end] == needle[..])
        })),
        "padStart" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let len = s.chars().count() as i64;
            let target = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() || x <= 0.0 {
                        0
                    } else if x.is_infinite() {
                        // ToLength(+∞) → 2^53-1 (practically: saturate)
                        i64::MAX / 2
                    } else {
                        x.floor() as i64
                    }
                }
                None => 0,
            };
            let needed = target - len;
            if needed <= 0 {
                return Value::string(s.to_string());
            }
            let pad: String = match args.get(1) {
                Some(v) => to_string_js(v),
                None => " ".to_string(),
            };
            if pad.is_empty() {
                return Value::string(s.to_string());
            }
            let fill: String = pad.chars().cycle().take(needed as usize).collect();
            Value::string(format!("{}{}", fill, s))
        })),
        "padEnd" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let len = s.chars().count() as i64;
            let target = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() || x <= 0.0 {
                        0
                    } else if x.is_infinite() {
                        i64::MAX / 2
                    } else {
                        x.floor() as i64
                    }
                }
                None => 0,
            };
            let needed = target - len;
            if needed <= 0 {
                return Value::string(s.to_string());
            }
            let pad: String = match args.get(1) {
                Some(v) => to_string_js(v),
                None => " ".to_string(),
            };
            if pad.is_empty() {
                return Value::string(s.to_string());
            }
            let fill: String = pad.chars().cycle().take(needed as usize).collect();
            Value::string(format!("{}{}", s, fill))
        })),
        "indexOf" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            // ES String.prototype.indexOf: pos = ToIntegerOrInfinity(position)
            // (NaN/undefined → 0); +∞ → len, -∞ → 0, then clamp to [0, len].
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let start = if num_pos.is_nan() {
                0
            } else if num_pos.is_infinite() {
                if num_pos > 0.0 {
                    hay.len()
                } else {
                    0
                }
            } else {
                (num_pos.trunc().max(0.0) as i64).min(hay.len() as i64) as usize
            };
            match char_index_of(&hay, &needle, start) {
                Some(i) => Value::int(i as i64),
                None => Value::int(-1),
            }
        })),
        "lastIndexOf" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let n = hay.len();
            let needle: Vec<char> = match args.first() {
                Some(v) => to_string_js(v).chars().collect(),
                None => Vec::new(),
            };
            // ES String.prototype.lastIndexOf: NaN position → +∞ → len;
            // -∞ → -1; finite values truncate toward zero.
            let num_pos = match args.get(1) {
                Some(v) if v.is_undefined() => f64::NAN,
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            let m = needle.len();
            if m == 0 {
                // Empty needle: min(pos, len) — but -∞ (or negative) is -1 in
                // Node? No: `"abc".lastIndexOf("", -1)` → 0 (spec: min(pos, len)
                // after pos is clamped by the caller's +∞/finite rules).
                let pos = if num_pos.is_nan() {
                    n
                } else if num_pos.is_infinite() {
                    if num_pos > 0.0 {
                        n
                    } else {
                        0
                    }
                } else {
                    (num_pos.trunc().max(0.0) as i64).min(n as i64) as usize
                };
                return Value::int(pos as i64);
            }
            let pos: i64 = if num_pos.is_nan() {
                n as i64
            } else if num_pos.is_infinite() {
                if num_pos > 0.0 {
                    n as i64
                } else {
                    return Value::int(-1);
                }
            } else {
                num_pos.trunc() as i64
            };
            // Clamp only downward: "If pos + searchLen > len, set pos to
            // len - searchLen" — the search stays at-or-before the original
            // fromIndex (lastIndexOf("b", 3) on "abcabc" → 1, not 4).
            let start = if pos + m as i64 > n as i64 {
                (n - m) as i64
            } else {
                pos
            };
            let start = start.max(0) as usize;
            match char_last_index_of(&hay, &needle, start) {
                Some(i) => Value::int(i as i64),
                None => Value::int(-1),
            }
        })),
        "match" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("").to_string();
            let hay: Vec<char> = s.chars().collect();
            let re = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(st) = re.as_regex() {
                let st = st.clone();
                let global = st
                    .lock()
                    .unwrap_or_else(|g| g.into_inner())
                    .compiled
                    .flags
                    .global;
                if global {
                    // Array of all full-match texts (no captures/index/input),
                    // like Node.
                    let prog = st
                        .lock()
                        .unwrap_or_else(|g| g.into_inner())
                        .compiled
                        .clone();
                    let mut texts = Vec::new();
                    match regex::scan_all(&prog, &hay) {
                        Ok(ms) => {
                            for (a, b, _) in ms {
                                texts.push(Value::string(hay[a..b].iter().collect()));
                            }
                            Value::array(texts)
                        }
                        Err(e) => {
                            vm.throw_exception(Value::string(format!(
                                "SyntaxError: Invalid regular expression: {}",
                                e
                            )));
                            Value::null()
                        }
                    }
                } else {
                    // Exec-like: match object or null. lastIndex is ignored
                    // for non-global match (Node behavior).
                    regex_exec_value(&st, &Value::string(s), vm)
                }
            } else {
                // String pattern: coerced to a non-global regex (Node).
                match regex::compile_from_str(&to_string_js(&re), "") {
                    Ok(prog) => {
                        let st = Arc::new(Mutex::new(RegexState {
                            compiled: Arc::new(prog),
                            last_index: 0,
                        }));
                        regex_exec_value(&st, &Value::string(s), vm)
                    }
                    Err(_) => Value::null(),
                }
            }
        })),
        "search" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let hay: Vec<char> = s.chars().collect();
            let re = args.first().cloned().unwrap_or(Value::undefined());
            // Node ignores lastIndex for search; a string arg is coerced to
            // a non-global regex.
            let prog: Arc<RegexCompiled> = if let Some(st) = re.as_regex() {
                st.lock()
                    .unwrap_or_else(|g| g.into_inner())
                    .compiled
                    .clone()
            } else {
                match regex::compile_from_str(&to_string_js(&re), "") {
                    Ok(p) => Arc::new(p),
                    Err(_) => return Value::int(-1),
                }
            };
            match regex::search(&prog, &hay, 0) {
                Ok(Some(m)) => Value::int(regex::char_pos_to_utf16(&hay, m.start) as i64),
                Ok(None) => Value::int(-1),
                Err(e) => {
                    vm.throw_exception(Value::string(format!(
                        "SyntaxError: Invalid regular expression: {}",
                        e
                    )));
                    Value::int(-1)
                }
            }
        })),
        "replace" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let repl = args.get(1).cloned().unwrap_or(Value::undefined());
            let hay: Vec<char> = s.chars().collect();
            // Regex search arg: full semantics (global -> all matches, else
            // first only; captures fed to the replacer function or `$n`).
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                let global = prog.flags.global;
                let matches: Vec<regex::Match> = if global {
                    match regex::scan_all(&prog, &hay) {
                        Ok(ms) => ms
                            .into_iter()
                            .map(|(a, b, caps)| regex::Match {
                                start: a,
                                end: b,
                                caps,
                            })
                            .collect(),
                        Err(e) => {
                            vm.throw_exception(Value::string(format!(
                                "SyntaxError: Invalid regular expression: {}",
                                e
                            )));
                            return Value::undefined();
                        }
                    }
                } else {
                    match regex::search(&prog, &hay, 0) {
                        Ok(Some(m)) => vec![m],
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            vm.throw_exception(Value::string(format!(
                                "SyntaxError: Invalid regular expression: {}",
                                e
                            )));
                            return Value::undefined();
                        }
                    }
                };
                let mut out = String::new();
                let mut pos = 0usize;
                let hay_text = s.to_string();
                for m in &matches {
                    let a = m.start;
                    let b = m.end;
                    out.push_str(&hay[pos..a].iter().collect::<String>());
                    let matched: String = hay[a..b].iter().collect();
                    let r = if let Some(f) = repl.as_function() {
                        // Replacer: (match, ...captures, offset, string).
                        let n_groups = (m.caps.len().saturating_sub(1)) / 2;
                        let mut fargs: Vec<Value> = Vec::with_capacity(n_groups + 3);
                        fargs.push(Value::string(matched.clone()));
                        for g in 1..=n_groups {
                            match m.caps.get(g).and_then(|c| *c) {
                                Some((x, y)) => {
                                    fargs.push(Value::string(hay[x..y].iter().collect()))
                                }
                                None => fargs.push(Value::undefined()),
                            }
                        }
                        fargs.push(Value::int(regex::char_pos_to_utf16(&hay, a) as i64));
                        fargs.push(Value::string(hay_text.clone()));
                        to_string_js(&vm.call_value(&Value::function(f.clone()), &fargs))
                    } else {
                        let before_str: String = hay[..a].iter().collect();
                        let after_str: String = hay[b..].iter().collect();
                        expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before_str,
                            &after_str,
                            &m.caps,
                            &hay,
                        )
                    };
                    out.push_str(&r);
                    if b > a {
                        pos = b;
                    } else {
                        // Empty match: advance one char like the scanner.
                        pos = (a + 1).min(hay.len());
                    }
                }
                out.push_str(&hay[pos..].iter().collect::<String>());
                return Value::string(out);
            }
            // String search arg: literal first-occurrence replace (Node
            // replaces only the first occurrence for a string pattern).
            let search: String = match args.first() {
                Some(v) => to_string_js(v),
                None => String::new(),
            };
            let needle: Vec<char> = search.chars().collect();
            match char_index_of(&hay, &needle, 0) {
                None => Value::string(s.to_string()),
                Some(i) => {
                    let before: String = hay[..i].iter().collect();
                    let after: String = hay[i + needle.len()..].iter().collect();
                    let matched: String = hay[i..i + needle.len()].iter().collect();
                    if let Some(f) = repl.as_function() {
                        // No capture groups without regex: (match, offset, string).
                        let out = vm.call_value(
                            &Value::function(f.clone()),
                            &[
                                Value::string(matched.clone()),
                                Value::int(i as i64),
                                Value::string(s.to_string()),
                            ],
                        );
                        let r = to_string_js(&out);
                        Value::string(format!("{}{}{}", before, r, after))
                    } else {
                        let r = expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before,
                            &after,
                            &[],
                            &[],
                        );
                        Value::string(format!("{}{}{}", before, r, after))
                    }
                }
            }
        })),
        "charCodeAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let units: Vec<u16> = s.encode_utf16().collect();
            if i < 0 || i as usize >= units.len() {
                Value::number(f64::NAN)
            } else {
                Value::int(units[i as usize] as i64)
            }
        })),
        "codePointAt" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let units: Vec<u16> = s.encode_utf16().collect();
            if i < 0 || i as usize >= units.len() {
                return Value::undefined();
            }
            let u = units[i as usize];
            let cp: u32 = if (0xD800..=0xDBFF).contains(&u) && (i as usize) + 1 < units.len() {
                let lo = units[i as usize + 1];
                if (0xDC00..=0xDFFF).contains(&lo) {
                    0x10000 + ((u - 0xD800) as u32) * 0x400 + (lo - 0xDC00) as u32
                } else {
                    u as u32
                }
            } else {
                u as u32
            };
            Value::int(cp as i64)
        })),
        // `toLowerCase` / `toLocaleLowerCase` — the missing counterpart to
        // `toUpperCase` (Unicode lowercase, ASCII-exact for the corpus).
        "toLowerCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_lowercase())
        })),
        "toLocaleLowerCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_lowercase())
        })),
        "toLocaleUpperCase" => Value::native(Arc::new(move |_args, _vm| {
            Value::string(s.as_str().unwrap_or("").to_uppercase())
        })),
        // `repeat(n)`: the string repeated n times; negative/fractional n is
        // a RangeError, NaN/0 → "".
        "repeat" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let n = match args.first() {
                Some(v) => v.to_number(),
                None => f64::NAN,
            };
            if n.is_nan() || n == 0.0 {
                return Value::string(String::new());
            }
            if n < 0.0 || n.is_infinite() {
                vm.throw_exception(Value::string("RangeError: Invalid count value".to_string()));
                return Value::undefined();
            }
            let count = n.floor() as usize;
            if s.len() * count > (1usize << 28) {
                vm.throw_exception(Value::string(
                    "RangeError: Invalid string length".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(s.repeat(count))
        })),
        // `at(i)`: UTF-16 code-unit index, negative counts from the end.
        "at" => Value::native(Arc::new(move |args, _vm| {
            let s = s.as_str().unwrap_or("");
            let units: Vec<u16> = s.encode_utf16().collect();
            let n = units.len() as i64;
            let i = match args.first() {
                Some(v) => {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                }
                None => 0,
            };
            let idx = if i < 0 { n + i } else { i };
            if idx < 0 || idx as usize >= units.len() {
                return Value::undefined();
            }
            // Return the code point that starts at this unit: a lone high
            // surrogate is emitted on its own (JS String.raw behavior via
            // String.fromCharCode semantics).
            let cp = units[idx as usize];
            if (0xD800..=0xDBFF).contains(&cp) && (idx as usize) + 1 < units.len() {
                let lo = units[idx as usize + 1];
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let full = 0x10000 + ((cp - 0xD800) as u32) * 0x400 + (lo - 0xDC00) as u32;
                    if let Some(c) = char::from_u32(full) {
                        return Value::char_str_utf8(c);
                    }
                }
            }
            if let Some(c) = char::from_u32(cp as u32) {
                Value::char_str_utf8(c)
            } else {
                Value::string(String::new())
            }
        })),
        // `replaceAll(search, repl)`: like replace but replaces EVERY
        // occurrence; a non-global regex search is a TypeError.
        "replaceAll" => Value::native(Arc::new(move |args, vm| {
            let s = s.as_str().unwrap_or("");
            let repl = args.get(1).cloned().unwrap_or(Value::undefined());
            // Regex search: must be /g, else TypeError (Node).
            if let Some(st) = args.first().and_then(|v| v.as_regex()).cloned() {
                let g = st.lock().unwrap_or_else(|g| g.into_inner());
                if !g.compiled.flags.global {
                    vm.throw_exception(Value::string("TypeError: String.prototype.replaceAll called with a non-global RegExp argument".to_string()));
                    return Value::undefined();
                }
                drop(g);
                // Reuse the replace path with a global scan.
                let hay: Vec<char> = s.chars().collect();
                let prog = {
                    let g = st.lock().unwrap_or_else(|g| g.into_inner());
                    g.compiled.clone()
                };
                let matches: Vec<regex::Match> = match regex::scan_all(&prog, &hay) {
                    Ok(ms) => ms
                        .into_iter()
                        .map(|(a, b, caps)| regex::Match {
                            start: a,
                            end: b,
                            caps,
                        })
                        .collect(),
                    Err(e) => {
                        vm.throw_exception(Value::string(format!(
                            "SyntaxError: Invalid regular expression: {}",
                            e
                        )));
                        return Value::undefined();
                    }
                };
                let mut out = String::new();
                let mut pos = 0usize;
                let hay_text = s.to_string();
                for m in &matches {
                    let a = m.start;
                    let b = m.end;
                    out.push_str(&hay[pos..a].iter().collect::<String>());
                    let matched: String = hay[a..b].iter().collect();
                    let r = if let Some(f) = repl.as_function() {
                        let n_groups = (m.caps.len().saturating_sub(1)) / 2;
                        let mut fargs: Vec<Value> = Vec::with_capacity(n_groups + 3);
                        fargs.push(Value::string(matched.clone()));
                        for g in 1..=n_groups {
                            match m.caps.get(g).and_then(|c| *c) {
                                Some((x, y)) => {
                                    fargs.push(Value::string(hay[x..y].iter().collect()))
                                }
                                None => fargs.push(Value::undefined()),
                            }
                        }
                        fargs.push(Value::int(regex::char_pos_to_utf16(&hay, a) as i64));
                        fargs.push(Value::string(hay_text.clone()));
                        to_string_js(&vm.call_value(&Value::function(f.clone()), &fargs))
                    } else {
                        let before_str: String = hay[..a].iter().collect();
                        let after_str: String = hay[b..].iter().collect();
                        expand_replacement(
                            &to_string_js(&repl),
                            &matched,
                            &before_str,
                            &after_str,
                            &m.caps,
                            &hay,
                        )
                    };
                    out.push_str(&r);
                    if b > a {
                        pos = b;
                    } else {
                        pos = (a + 1).min(hay.len());
                    }
                }
                out.push_str(&hay[pos..].iter().collect::<String>());
                return Value::string(out);
            }
            // String search: replace every non-overlapping occurrence.
            let search: String = match args.first() {
                Some(v) => to_string_js(v),
                None => String::new(),
            };
            if search.is_empty() {
                // Empty needle: insert between every char (and at both ends).
                let r = to_string_js(&repl);
                let chars: Vec<char> = s.chars().collect();
                let mut out = String::new();
                for (i, c) in chars.iter().enumerate() {
                    out.push_str(&r);
                    out.push(*c);
                    if i == chars.len() - 1 {
                        out.push_str(&r);
                    }
                }
                if chars.is_empty() {
                    return Value::string(r);
                }
                return Value::string(out);
            }
            let needle: Vec<char> = search.chars().collect();
            let hay: Vec<char> = s.chars().collect();
            let mut out = String::new();
            let mut pos = 0usize;
            while pos <= hay.len() {
                match char_index_of(&hay, &needle, pos) {
                    Some(i) => {
                        out.push_str(&hay[pos..i].iter().collect::<String>());
                        let matched: String = hay[i..i + needle.len()].iter().collect();
                        if let Some(f) = repl.as_function() {
                            let r = vm.call_value(
                                &Value::function(f.clone()),
                                &[
                                    Value::string(matched.clone()),
                                    Value::int(i as i64),
                                    Value::string(s.to_string()),
                                ],
                            );
                            out.push_str(&to_string_js(&r));
                        } else {
                            let before_str: String = hay[..i].iter().collect();
                            let after_str: String = hay[i + needle.len()..].iter().collect();
                            out.push_str(&expand_replacement(
                                &to_string_js(&repl),
                                &matched,
                                &before_str,
                                &after_str,
                                &[],
                                &[],
                            ));
                        }
                        pos = i + needle.len();
                    }
                    None => {
                        out.push_str(&hay[pos..].iter().collect::<String>());
                        break;
                    }
                }
            }
            Value::string(out)
        })),
        "\0sym_1" => Value::native(Arc::new(move |_args, _vm| {
            let str_val = s.as_str().unwrap_or("");
            let chars: Vec<Value> = str_val
                .chars()
                .map(|c| Value::string(c.to_string()))
                .collect();
            super::arrays::make_array_iterator(chars)
        })),
        _ => Value::undefined(),
    }
}

pub(crate) fn make_string_module() -> Value {
    let from_char_code = Value::native(Arc::new(|args, _vm| {
        let mut out = String::new();
        for a in args {
            let n = a.to_number();
            let n = if n.is_nan() || n <= 0.0 {
                0.0
            } else {
                n.trunc()
            };
            let n = ((n as i64) & 0xFFFF) as u32;
            if let Some(c) = char::from_u32(n) {
                out.push(c);
            }
        }
        Value::string(out)
    }));
    let from_code_point = Value::native(Arc::new(|args, _vm| {
        let mut out = String::new();
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                out.push('\u{FFFD}');
            } else if n < 0.0 || n > 0x10FFFF as f64 || (n.trunc() != n) {
                // Invalid code point: engine throws via the host's next
                // native check — coerce to the replacement char instead.
                out.push('\u{FFFD}');
            } else if let Some(c) = char::from_u32(n as u32) {
                out.push(c);
            } else {
                out.push('\u{FFFD}');
            }
        }
        Value::string(out)
    }));
    let raw = Value::native(Arc::new(|args, vm| {
        let template = args.first().cloned().unwrap_or(Value::undefined());
        if template.is_null() || template.is_undefined() {
            vm.throw_exception(Value::string(
                "TypeError: Cannot convert undefined or null to object".to_string(),
            ));
            return Value::undefined();
        }
        let raw_val = if let Some(od) = template.as_object() {
            od.borrow()
                .get("raw")
                .cloned()
                .unwrap_or(Value::undefined())
        } else {
            Value::undefined()
        };
        let raw_arr = match raw_val.as_array() {
            Some(a) => a.borrow().to_values(),
            None => {
                vm.throw_exception(Value::string(
                    "TypeError: Cannot convert undefined or null to object".to_string(),
                ));
                return Value::undefined();
            }
        };
        let raw_len = raw_arr.len();
        if raw_len == 0 {
            return Value::string(String::new());
        }
        let mut out = String::new();
        for i in 0..raw_len {
            if i > 0 {
                if let Some(sub) = args.get(i) {
                    out.push_str(&to_string_js(sub));
                }
            }
            out.push_str(&to_string_js(&raw_arr[i]));
        }
        Value::string(out)
    }));
    // Callable `String(x)` coercion plus statics (`String.fromCharCode`,
    // `String.fromCodePoint`, `String.raw`) on the same value via native props.
    Value::native_with_props(
        Arc::new(move |args, _vm| match args.first() {
            Some(v) => Value::string(to_string_js(v)),
            None => Value::string(String::new()),
        }),
        Value::undefined(),
        vec![
            ("fromCharCode".to_string(), from_char_code),
            ("fromCodePoint".to_string(), from_code_point),
            ("raw".to_string(), raw),
        ],
    )
}
