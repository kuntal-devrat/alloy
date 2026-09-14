use crate::vm::core::unwrap_cell;
use alloy_core::value::{to_string_js, Value, VmHost};
use hashbrown::HashMap;
use std::sync::Arc;

pub(crate) fn json_stringify_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `replacer` argument of JSON.stringify: no filter, a key whitelist
/// (array of strings/numbers, applied to every object at every nesting level
/// — arrays always pass all elements), or a function called as `(key, value)`
/// for the root and every property/element before serialization.
#[derive(Clone)]
enum JsonReplacer {
    None,
    Keys(Vec<String>),
    Func(Value),
}

/// Serialize `v` per JSON.stringify. Returns None for values that collapse to
/// undefined (undefined/function at the top level — inside arrays they become
/// null and inside objects the entry is omitted). `visited` is the cycle
/// stack: re-entering a box currently being serialized throws (JS TypeError).
/// `key` is the property name for the function replacer's first argument (""
/// at the root).
fn json_serialize(
    v: &Value,
    key: &str,
    depth: usize,
    indent: &str,
    replacer: &JsonReplacer,
    visited: &mut Vec<u64>,
    vm: &mut dyn VmHost,
) -> Option<String> {
    if depth > 512 {
        vm.throw_exception(Value::string(
            "TypeError: JSON structure too deeply nested".to_string(),
        ));
        return None;
    }
    // Function replacer: transform (or drop) every value before serializing,
    // including the root (key ""). Undefined/function results collapse the
    // same way as the source value would.
    let v = match replacer {
        JsonReplacer::Func(f) => {
            let r = vm.call_value(f, &[Value::string(key.to_string()), v.clone()]);
            if r.is_undefined() {
                return None;
            }
            r
        }
        _ => v.clone(),
    };
    if v.is_undefined() || v.as_function().is_some() || v.is_native() {
        return None;
    }
    if v.is_null() {
        return Some("null".to_string());
    }
    if let Some(b) = v.as_bool() {
        return Some(if b { "true" } else { "false" }.to_string());
    }
    if v.is_number() {
        return Some(alloy_core::value::number_to_string(v.to_number()));
    }
    if let Some(i) = v.as_int() {
        return Some(i.to_string());
    }
    if let Some(s) = v.as_str() {
        return Some(json_stringify_escape(s));
    }
    if v.is_array() {
        let id = v.bits();
        if visited.contains(&id) {
            vm.throw_exception(Value::string(
                "TypeError: Converting circular structure to JSON".to_string(),
            ));
            return None;
        }
        visited.push(id);
        let vals = v.as_array().unwrap().borrow().to_values();
        let mut parts: Vec<String> = Vec::with_capacity(vals.len());
        for (i, e) in vals.iter().enumerate() {
            match json_serialize(e, &i.to_string(), depth + 1, indent, replacer, visited, vm) {
                Some(s) => parts.push(s),
                None => parts.push("null".to_string()),
            }
        }
        visited.pop();
        if indent.is_empty() {
            return Some(format!("[{}]", parts.join(",")));
        }
        let pad = indent.repeat(depth);
        let pad_in = indent.repeat(depth + 1);
        return Some(format!(
            "[\n{}{}\n{}]",
            pad_in,
            parts.join(&format!(",\n{}", pad_in)),
            pad
        ));
    }
    if let Some(od) = v.as_object() {
        let to_json_opt = {
            let od_b = od.borrow();
            if od_b.container == alloy_core::value::DATE_CONTAINER {
                if let Some(ms) = od_b
                    .get(alloy_core::value::DATE_MS_KEY)
                    .map(|x| x.to_number())
                {
                    if !ms.is_finite() {
                        return Some("null".to_string());
                    }
                    return Some(format!("\"{}\"", alloy_core::value::date_to_iso_string(ms)));
                }
            }
            od_b.get("toJSON").cloned().or_else(|| {
                let mut curr = od_b.proto.clone();
                while let Some(p) = curr.clone().as_object() {
                    let pb = p.borrow();
                    if let Some(f) = pb.get("toJSON") {
                        return Some(f.clone());
                    }
                    curr = pb.proto.clone();
                }
                None
            })
        };
        if let Some(f) = to_json_opt {
            if f.as_function().is_some() || f.is_native() {
                let transformed =
                    vm.call_value_with_this(&f, Some(v.clone()), &[Value::string(key.to_string())]);
                return json_serialize(&transformed, key, depth, indent, replacer, visited, vm);
            }
        }

        let id = v.bits();
        if visited.contains(&id) {
            vm.throw_exception(Value::string(
                "TypeError: Converting circular structure to JSON".to_string(),
            ));
            return None;
        }
        visited.push(id);
        let od = od.borrow();
        let entries: Vec<(&String, u32)> = od.shape.keys_by_offset();
        let mut parts: Vec<String> = Vec::new();
        for (k, off) in entries {
            if od.deleted[off as usize] || k.starts_with('\0') {
                continue;
            }
            // Key whitelist: applies to objects at every depth.
            if let JsonReplacer::Keys(keys) = replacer {
                if !keys.iter().any(|s| s == k) {
                    continue;
                }
            }
            // Live-import cells serialize as their current value (an exports
            // object's properties are the module's own storage).
            let val = unwrap_cell(od.values[off as usize].clone());
            if let Some(s) = json_serialize(&val, k, depth + 1, indent, replacer, visited, vm) {
                parts.push(format!(
                    "{}:{}{}",
                    json_stringify_escape(k),
                    if indent.is_empty() { "" } else { " " },
                    s
                ))
            }
        }
        visited.pop();
        if indent.is_empty() {
            return Some(format!("{{{}}}", parts.join(",")));
        }
        let pad = indent.repeat(depth);
        let pad_in = indent.repeat(depth + 1);
        return Some(format!(
            "{{\n{}{}\n{}}}",
            pad_in,
            parts.join(&format!(",\n{}", pad_in)),
            pad
        ));
    }
    None
}

/// The `space` argument: a number indents that many spaces (capped at 10), a
/// string is used verbatim (capped at 10 chars), anything else → no pretty
/// printing.
pub(crate) fn json_space(space: &Value) -> String {
    // An int literal (`2`) is a tagged int, not an f64 — check both.
    if space.as_int().is_some() || space.is_number() {
        let n = space.to_number();
        if n.is_finite() && n > 0.0 {
            let k = (n.floor() as usize).min(10);
            " ".repeat(k)
        } else {
            String::new()
        }
    } else if let Some(s) = space.as_str() {
        s.chars().take(10).collect()
    } else {
        String::new()
    }
}

/// JSON.parse: a small recursive-descent parser over `char`s (RFC 8259
/// grammar — no NaN/Infinity/leading zeros/single quotes/trailing commas/
/// unquoted keys; `\uXXXX` escapes incl. surrogate pairs).
pub(crate) struct JsonParser {
    chars: Vec<char>,
    i: usize,
    depth: usize,
}

impl JsonParser {
    fn ws(&mut self) {
        while self.i < self.chars.len() && matches!(self.chars[self.i], ' ' | '\t' | '\n' | '\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.next() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected '{}'", c))
        }
    }

    fn lit(&mut self, s: &str) -> Result<(), String> {
        for c in s.chars() {
            if self.next() != Some(c) {
                return Err(format!("expected '{}'", s));
            }
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut n = 0u32;
        for _ in 0..4 {
            match self.next().and_then(|c| c.to_digit(16)) {
                Some(d) => n = n * 16 + d,
                None => return Err("bad \\u escape".to_string()),
            }
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.next() {
                Some('"') => return Ok(out),
                Some('\\') => match self.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{C}'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('u') => {
                        let hi = self.hex4()?;
                        if (0xD800..=0xDBFF).contains(&hi) {
                            // Surrogate pair: \uD800-\uDBFF \uDC00-\uDFFF.
                            let save = self.i;
                            if self.next() == Some('\\') && self.next() == Some('u') {
                                let lo = self.hex4()?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                } else {
                                    self.i = save;
                                    out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                                }
                            } else {
                                self.i = save;
                                out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        } else {
                            out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                        }
                    }
                    _ => return Err("bad escape".to_string()),
                },
                Some(c) if (c as u32) < 0x20 => return Err("control char in string".to_string()),
                Some(c) => out.push(c),
                None => return Err("unterminated string".to_string()),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some('-') {
            self.i += 1;
        }
        match self.peek() {
            Some('0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err("bad number".to_string()),
        }
        if self.peek() == Some('.') {
            self.i += 1;
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err("bad number fraction".to_string());
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            self.i += 1;
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.i += 1;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err("bad number exponent".to_string());
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let s: String = self.chars[start..self.i].iter().collect();
        let n: f64 = s.parse().unwrap_or(f64::NAN);
        // Integral and safe → int; -0 and fractions stay numbers.
        if n.fract() == 0.0 && n.abs() <= 9007199254740992.0 && !(n == 0.0 && s.starts_with('-')) {
            Ok(Value::int(n as i64))
        } else {
            Ok(Value::number(n))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        self.depth += 1;
        if self.depth > 512 {
            return Err("JSON structure too deeply nested".to_string());
        }
        let res = match self.peek() {
            Some('{') => self.obj(),
            Some('[') => self.arr(),
            Some('"') => Ok(Value::string(self.string()?)),
            Some('t') => {
                self.lit("true")?;
                Ok(Value::bool(true))
            }
            Some('f') => {
                self.lit("false")?;
                Ok(Value::bool(false))
            }
            Some('n') => {
                self.lit("null")?;
                Ok(Value::null())
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => Err("unexpected character".to_string()),
        };
        self.depth -= 1;
        res
    }

    fn arr(&mut self) -> Result<Value, String> {
        self.expect('[')?;
        self.ws();
        let mut out: Vec<Value> = Vec::new();
        if self.peek() == Some(']') {
            self.i += 1;
            return Ok(Value::array(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            match self.next() {
                Some(',') => self.ws(),
                Some(']') => break,
                _ => return Err("expected ',' or ']'".to_string()),
            }
        }
        Ok(Value::array(out))
    }

    fn obj(&mut self) -> Result<Value, String> {
        self.expect('{')?;
        self.ws();
        let mut keys: Vec<String> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();
        if self.peek() == Some('}') {
            self.i += 1;
            return Ok(Value::object(hashbrown::HashMap::new()));
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err("expected string key".to_string());
            }
            let k = self.string()?;
            self.ws();
            self.expect(':')?;
            vals.push(self.value()?);
            keys.push(k);
            self.ws();
            match self.next() {
                Some(',') => {}
                Some('}') => break,
                _ => return Err("expected ',' or '}'".to_string()),
            }
        }
        // Keys arrive in document order — preserve it for round-trip
        // stringify (JS objects keep insertion order).
        Ok(Value::object_ordered(keys.into_iter().zip(vals).collect()))
    }

    fn parse(&mut self) -> Result<Value, String> {
        let v = self.value()?;
        self.ws();
        if self.i != self.chars.len() {
            return Err("trailing characters".to_string());
        }
        Ok(v)
    }
}

pub fn parse_json_str(s: &str) -> Result<Value, String> {
    JsonParser {
        chars: s.chars().collect(),
        i: 0,
        depth: 0,
    }
    .parse()
}

/// `JSON` global: stringify (with the `space` pretty-print arg, cycle
/// detection that throws, functions/undefined collapsing) and parse (reviver
/// ignored; syntax errors throw). The throw path runs through the new
/// `VmHost::throw_exception` hook so try/catch catches it like a `throw`.
pub(crate) fn make_json_module() -> Value {
    let stringify = Value::native(Arc::new(move |args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        let space = args.get(2).cloned().unwrap_or(Value::undefined());
        let indent = json_space(&space);
        let replacer = match args.get(1) {
            Some(r) if r.as_array().is_some() => {
                let ad = r.as_array().unwrap().borrow();
                let mut keys: Vec<String> = Vec::new();
                for e in ad.to_values() {
                    // Only strings and numbers count (numbers stringified).
                    if e.as_str().is_some() || e.as_int().is_some() || e.is_number() {
                        keys.push(to_string_js(&e));
                    }
                }
                JsonReplacer::Keys(keys)
            }
            Some(r) if !r.is_undefined() && !r.is_null() => JsonReplacer::Func(r.clone()),
            _ => JsonReplacer::None,
        };
        let mut visited: Vec<u64> = Vec::new();
        match json_serialize(&v, "", 0, &indent, &replacer, &mut visited, vm) {
            Some(s) => Value::string(s),
            None => Value::undefined(),
        }
    }));
    let parse = Value::native(Arc::new(move |args, vm| {
        let text = args.first().cloned().unwrap_or(Value::undefined());
        let s = match text.as_str() {
            Some(s) => s.to_string(),
            None => to_string_js(&text),
        };
        let mut p = JsonParser {
            chars: s.chars().collect(),
            i: 0,
            depth: 0,
        };
        match p.parse() {
            Ok(v) => v,
            Err(msg) => {
                vm.throw_exception(Value::string(format!("SyntaxError: {}", msg)));
                Value::undefined()
            }
        }
    }));
    let mut m = HashMap::new();
    m.insert("stringify".to_string(), stringify);
    m.insert("parse".to_string(), parse);
    Value::object(m)
}

pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn serialize_value(v: &Value) -> String {
    if v.is_undefined() {
        "undefined".to_string()
    } else if v.is_null() {
        "null".to_string()
    } else if let Some(b) = v.as_bool() {
        b.to_string()
    } else if let Some(n) = v.as_number() {
        if !n.is_finite() {
            "null".to_string()
        } else if n == (n as i64) as f64 {
            (n as i64).to_string()
        } else {
            n.to_string()
        }
    } else if let Some(i) = v.as_int() {
        i.to_string()
    } else if let Some(s) = v.as_str() {
        format!("\"{}\"", json_escape(s))
    } else if let Some(a) = v.as_array() {
        let a = a.borrow();
        let inner: Vec<String> = a.to_values().iter().map(serialize_value).collect();
        format!("[{}]", inner.join(", "))
    } else if let Some(m) = v.as_object() {
        let m = m.borrow();
        let inner: Vec<String> = m
            .iter_sorted()
            .into_iter()
            .map(|(k, val)| format!("\"{}\": {}", k, serialize_value(val)))
            .collect();
        format!("{{{}}}", inner.join(", "))
    } else {
        "\"native\"".to_string()
    }
}
