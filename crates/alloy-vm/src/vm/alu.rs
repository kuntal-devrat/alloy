use crate::opcode::Opcode;
use alloy_core::value::Value;
use super::builtins::containers::container_pairs;
use super::stack::{OperandStack, KIND_INT, KIND_NUMBER};

/// Expand spread positions in a value list (in source order): each position
/// whose mask bit is set holds an array whose elements are spliced in place.
pub(crate) fn expand_spreads(vals: Vec<Value>, mask: u16) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, v) in vals.into_iter().enumerate() {
        if mask & (1 << i) != 0 {
            if let Some(a) = v.as_array() {
                let a = a.borrow();
                out.extend(a.to_values());
            } else if let Some(s) = v.as_str() {
                // Strings are iterable in JS: spread by character.
                out.extend(s.chars().map(|c| Value::string(c.to_string())));
            } else if let Some(od) = v.as_object() {
                // Map/Set are iterable: a Map spreads its [k, v] entry pairs,
                // a Set its elements — both in insertion order.
                let c = od.borrow().container;
                if c == 1 || c == 2 {
                    for (k, val) in container_pairs(&v) {
                        if c == 1 {
                            out.push(Value::array(vec![k, val]));
                        } else {
                            out.push(val);
                        }
                    }
                } else {
                    out.push(v);
                }
            } else {
                out.push(v);
            }
        } else {
            out.push(v);
        }
    }
    out
}

/// Apply a fused arith code (0=Add 1=Sub 2=Mul 3=Div 4=Mod 5=BitAnd
/// 6=BitOr 7=BitXor 8=Shl 9=Shr 10=UShr 11=Pow). Mirrors the Add/Subtract/
/// Multiply/Divide/Modulo/BitAnd/BitOr/BitXor/Shl/Shr/UShr/Pow opcode
/// semantics exactly.
pub(crate) fn arith_apply(l: &Value, r: &Value, ar: u8) -> Value {
    match ar {
        0 => l.add(r),
        1 => l.subtract(r),
        2 => l.multiply(r),
        3 => l.divide(r),
        5 => l.bitand(r),
        6 => l.bitor(r),
        7 => l.bitxor(r),
        8 => l.shl(r),
        9 => l.shr(r),
        10 => l.ushr(r),
        11 => l.pow(r),
        4 => l.modulo(r),
        _ => Value::undefined(),
    }
}

/// The ArithChain i64 fast lane: `a ar b` when both are ints, with the same
/// edge cases as [`Value::add`]/[`subtract`]/[`multiply`]/[`divide`]/
/// [`modulo`] (overflow → f64, `0 * -5` → -0, `% 0` → NaN, `-9 % 3` → -0,
/// `/` always f64). Returns None for ar codes without an int fast lane
/// (bitwise/shift/pow) — the caller falls back to `arith_apply`, so the chain
/// is exactly equivalent to the sequence of plain opcodes.
#[inline(always)]
pub(crate) fn chain_arith_i64(a: i64, b: i64, ar: u8) -> Option<Value> {
    Some(match ar {
        0 => match a.checked_add(b) {
            Some(r) => Value::int(r),
            None => Value::number(a as f64 + b as f64),
        },
        1 => match a.checked_sub(b) {
            Some(r) => Value::int(r),
            None => Value::number(a as f64 - b as f64),
        },
        2 => match a.checked_mul(b) {
            Some(0) if (a < 0) != (b < 0) => Value::number(-0.0),
            Some(r) => Value::int(r),
            None => Value::number(a as f64 * b as f64),
        },
        3 => Value::number(a as f64 / b as f64),
        4 => {
            if b == 0 {
                Value::number(f64::NAN)
            } else if a % b == 0 && a < 0 {
                Value::number(-0.0)
            } else {
                Value::int(a % b)
            }
        }
        _ => return None,
    })
}

/// One register-ALU step: `l ar b` (b raw i64) with the int fast lane and
/// the generic Value fallback — used by the fixed-shape superinstructions.
#[inline(always)]
pub(crate) fn chain_step_i64(l: &Value, b: i64, ar: u8) -> Value {
    if let Some(a) = l.as_int() {
        if let Some(res) = chain_arith_i64(a, b, ar) {
            return res;
        }
    }
    arith_apply(l, &Value::int(b), ar)
}

/// f64 fast lane for `a ar b` (b as f64): mirrors the (number, int)/(number,
/// number) branches of the Value ops exactly — no ToNumber, no string probe.
/// Returns None for ar codes without an f64 lane (bitwise/shift/pow coerce
/// via ToInt32 and must fall back to the generic Value op).
#[inline(always)]
pub(crate) fn f64_lane(a: f64, b: f64, ar: u8) -> Option<Value> {
    Some(match ar {
        0 => Value::number(a + b),
        1 => Value::number(a - b),
        2 => Value::number(a * b),
        3 => Value::number(a / b),
        4 => Value::number(a % b),
        _ => return None,
    })
}

/// Fast lane for `slots[slot] ar imm` when the slot's feedback kind is INT
/// or NUMBER: pure i64/f64 math, zero tag probes. `fast=false` means the
/// caller must use the generic Value path (unknown/other kind, or an ar code
/// with no lane for that kind).
#[inline(always)]
pub(crate) fn alu_local_imm(stack: &OperandStack, idx: usize, imm: i64, ar: u8) -> (Value, bool) {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                if let Some(res) = chain_arith_i64(Value::int_bits_raw(stack.at(idx).bits()), imm, ar)
                {
                    return (res, true);
                }
            }
            KIND_NUMBER => {
                if let Some(res) = f64_lane(f64::from_bits(stack.at(idx).bits()), imm as f64, ar) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Fast lane for `imm ar slots[slot]` (constant on the left — `3 * n`).
#[inline(always)]
pub(crate) fn alu_imm_local(stack: &OperandStack, idx: usize, imm: i64, ar: u8) -> (Value, bool) {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                if let Some(res) = chain_arith_i64(imm, Value::int_bits_raw(stack.at(idx).bits()), ar)
                {
                    return (res, true);
                }
            }
            KIND_NUMBER => {
                if let Some(res) = f64_lane(imm as f64, f64::from_bits(stack.at(idx).bits()), ar) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Fast lane for `slots[a] ar slots[b]` when both feedback kinds are known.
#[inline(always)]
pub(crate) fn alu_local_local(stack: &OperandStack, ia: usize, ib: usize, ar: u8) -> (Value, bool) {
    if ia < stack.len() && ib < stack.len() {
        match (stack.kind_of(ia), stack.kind_of(ib)) {
            (KIND_INT, KIND_INT) => {
                if let Some(res) = chain_arith_i64(
                    Value::int_bits_raw(stack.at(ia).bits()),
                    Value::int_bits_raw(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_NUMBER, KIND_INT) => {
                if let Some(res) = f64_lane(
                    f64::from_bits(stack.at(ia).bits()),
                    Value::int_bits_raw(stack.at(ib).bits()) as f64,
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_INT, KIND_NUMBER) => {
                if let Some(res) = f64_lane(
                    Value::int_bits_raw(stack.at(ia).bits()) as f64,
                    f64::from_bits(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            (KIND_NUMBER, KIND_NUMBER) => {
                if let Some(res) = f64_lane(
                    f64::from_bits(stack.at(ia).bits()),
                    f64::from_bits(stack.at(ib).bits()),
                    ar,
                ) {
                    return (res, true);
                }
            }
            _ => {}
        }
    }
    (Value::undefined(), false)
}

/// Two-step `(slots[slot] ar1 imm1) ar2 imm2` entirely inside one lane when
/// the slot's kind is known — `seed = (seed * 48271) % 2147483648`. Both
/// steps stay in i64 for INT slots and f64 for NUMBER slots, matching the
/// generic chain exactly. None = fall back to the generic path.
#[inline(always)]
pub(crate) fn alu2_local_imm_imm(
    stack: &OperandStack,
    idx: usize,
    imm1: i64,
    ar1: u8,
    imm2: i64,
    ar2: u8,
) -> Option<Value> {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                let r1 = chain_arith_i64(Value::int_bits_raw(stack.at(idx).bits()), imm1, ar1)?;
                let r2 = chain_arith_i64(r1.as_int()?, imm2, ar2)?;
                return Some(r2);
            }
            KIND_NUMBER => {
                let r1 = f64_lane(f64::from_bits(stack.at(idx).bits()), imm1 as f64, ar1)?;
                let r2 = f64_lane(r1.as_number()?, imm2 as f64, ar2)?;
                return Some(r2);
            }
            _ => {}
        }
    }
    None
}

/// Two-step `(imm1 ar1 slots[slot]) ar2 imm2` — `n = 3 * n + 1` (constant
/// init on the left).
#[inline(always)]
pub(crate) fn alu2_imm_local_imm(
    stack: &OperandStack,
    idx: usize,
    imm1: i64,
    ar1: u8,
    imm2: i64,
    ar2: u8,
) -> Option<Value> {
    if idx < stack.len() {
        match stack.kind_of(idx) {
            KIND_INT => {
                let r1 = chain_arith_i64(imm1, Value::int_bits_raw(stack.at(idx).bits()), ar1)?;
                let r2 = chain_arith_i64(r1.as_int()?, imm2, ar2)?;
                return Some(r2);
            }
            KIND_NUMBER => {
                let r1 = f64_lane(imm1 as f64, f64::from_bits(stack.at(idx).bits()), ar1)?;
                let r2 = f64_lane(r1.as_number()?, imm2 as f64, ar2)?;
                return Some(r2);
            }
            _ => {}
        }
    }
    None
}

/// Numeric comparison in the i64 lane — identical results to
/// `compare_values` for (int, int) operands (|v| ≤ 2^47 is exactly
/// representable as f64).
#[inline(always)]
pub(crate) fn cmp_i64(a: i64, b: i64, cmp: u8) -> bool {
    match cmp {
        0 => a < b,
        1 => a <= b,
        2 => a > b,
        3 => a >= b,
        4 => a == b,
        5 => a != b,
        6 => a == b,
        7 => a != b,
        _ => false,
    }
}

/// Map a raw compare OPCODE byte (Equal=16 … StrictNotEqual=90, as emitted
/// by the generic `Expr::Bin` path) to the semantic cmp code (0-7) that
/// `cmp_i64`/`compare_values` consume. The fused CmpLocal* opcodes already
/// carry the semantic code from the compiler; the generic-path fusions carry
/// the opcode ordinal and must translate.
#[inline(always)]
pub(crate) fn cmp_semantic(op: Opcode) -> u8 {
    match op {
        Opcode::Less => 0,
        Opcode::LessEqual => 1,
        Opcode::Greater => 2,
        Opcode::GreaterEqual => 3,
        Opcode::Equal => 4,
        Opcode::NotEqual => 5,
        Opcode::StrictEqual => 6,
        Opcode::StrictNotEqual => 7,
        _ => 6,
    }
}

/// Numeric comparison in the f64 lane — identical results to
/// `compare_values` for (number, int) operands (loose `==` and strict `===`
/// agree for two numbers).
#[inline(always)]
pub(crate) fn cmp_f64(a: f64, b: f64, cmp: u8) -> bool {
    match cmp {
        0 => a < b,
        1 => a <= b,
        2 => a > b,
        3 => a >= b,
        4 => a == b,
        5 => a != b,
        6 => a == b,
        7 => a != b,
        _ => false,
    }
}

pub(crate) fn strict_equal(l: &Value, r: &Value) -> bool {
    if let (Some(a), Some(b)) = (l.as_number(), r.as_number()) {
        a == b
    } else if let (Some(a), Some(b)) = (l.as_int(), r.as_int()) {
        a == b
    } else if let (Some(a), Some(b)) = (l.as_number(), r.as_int()) {
        a == b as f64
    } else if let (Some(a), Some(b)) = (l.as_int(), r.as_number()) {
        a as f64 == b
    } else if l.is_null() || l.is_undefined() {
        // Exact bit match: null === null / undefined === undefined only
        // (`null === undefined` is false even though `==` coerces them).
        l.bits() == r.bits()
    } else {
        l.same_type(r) && l.equal(r).is_truthy()
    }
}

/// Apply a fused compare code (0=< 1=<= 2=> 3=>= 4=== 5=!= 6=!== ...). Mirrors
/// the Less/Greater/LessEqual/GreaterEqual/Equal/NotEqual/StrictEqual
/// opcode semantics exactly (JS: string/string compares lexicographically,
/// everything else numerically; `!==` maps to loose NotEqual like the
/// compiler currently emits).
pub(crate) fn compare_values(l: &Value, r: &Value, cmp: u8) -> bool {
    // SMI fast path: int/int operands compare in the i64 lane — one tag check
    // per operand, no string probe, no ToNumber. Exact for every cmp code:
    // ints (|v| ≤ 2^47) are exactly representable as f64, so numeric ordering
    // and (in)equality agree with the JS ToNumber result. This is the hottest
    // path in loop-condition superinstructions like `i < 1000`.
    if let (Some(a), Some(b)) = (l.as_int(), r.as_int()) {
        return match cmp {
            0 => a < b,
            1 => a <= b,
            2 => a > b,
            3 => a >= b,
            4 => a == b,
            5 => a != b,
            6 => a == b,
            7 => a != b,
            _ => false,
        };
    }
    match cmp {
        0 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a < b
            } else {
                l.to_number() < r.to_number()
            }
        }
        1 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a <= b
            } else {
                l.to_number() <= r.to_number()
            }
        }
        2 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a > b
            } else {
                l.to_number() > r.to_number()
            }
        }
        3 => {
            if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                a >= b
            } else {
                l.to_number() >= r.to_number()
            }
        }
        4 => l.equal(r).is_truthy(),
        5 => !l.equal(r).is_truthy(),
        6 => strict_equal(l, r),
        7 => !strict_equal(l, r),
        _ => false,
    }
}
