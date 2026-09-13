use std::sync::Arc;
use hashbrown::HashMap;
use alloy_core::value::{to_string_js, js_number_to_string, Value};
use super::strings::js_trim;

// ---------------------------------------------------------------------------
// Number.prototype formatting — V8-exact toFixed / toPrecision / toString(radix)
// ---------------------------------------------------------------------------
// Rust's {:.*} rounds half-to-even on the decimal digits of the binary value;
// JS rounds the EXACT binary value half-away-from-zero (so (1.005).toFixed(2)
// is "1.00" and (2.5).toFixed(0) is "3"). toString(radix != 10) is the
// shortest-round-trip digit string (V8 prints "7b.74bc6a7ef9dc" for
// 123.456.toString(16), not the full 13-digit expansion). Everything below
// works on the exact dyadic value via a tiny arbitrary-precision integer.

type Big = Vec<u64>; // little-endian limbs

fn big_from_u64(v: u64) -> Big {
    vec![v]
}

fn big_trim(b: &mut Big) {
    while b.len() > 1 && *b.last().unwrap() == 0 {
        b.pop();
    }
}

fn big_is_zero(b: &Big) -> bool {
    b.iter().all(|&l| l == 0)
}

fn big_shl(b: &mut Big, bits: u64) {
    if bits == 0 {
        return;
    }
    let words = (bits / 64) as usize;
    let rem = (bits % 64) as u32;
    if words > 0 {
        let mut nb = vec![0u64; b.len() + words];
        nb[words..].copy_from_slice(b);
        *b = nb;
    }
    if rem > 0 {
        b.push(0);
        for i in (1..b.len()).rev() {
            b[i] = (b[i] << rem) | (b[i - 1] >> (64 - rem));
        }
        b[0] <<= rem;
    }
    big_trim(b);
}

fn big_shr(b: &mut Big, bits: u64) {
    if bits == 0 {
        return;
    }
    let words = (bits / 64) as usize;
    let rem = (bits % 64) as u32;
    if words >= b.len() {
        *b = vec![0];
        return;
    }
    if words > 0 {
        b.drain(..words);
    }
    if rem > 0 {
        for i in 0..b.len() - 1 {
            b[i] = (b[i] >> rem) | (b[i + 1] << (64 - rem));
        }
        *b.last_mut().unwrap() >>= rem;
    }
    big_trim(b);
}

fn big_mul_small(b: &mut Big, m: u64) {
    let mut carry = 0u64;
    for l in b.iter_mut() {
        let cur = (*l as u128) * (m as u128) + (carry as u128);
        *l = cur as u64;
        carry = (cur >> 64) as u64;
    }
    if carry > 0 {
        b.push(carry);
    }
}

fn big_add(a: &mut Big, b: &Big) {
    let mut carry = 0u64;
    let n = a.len().max(b.len());
    a.resize(n, 0);
    for i in 0..n {
        let av = a[i];
        let bv = if i < b.len() { b[i] } else { 0 };
        let (s1, c1) = av.overflowing_add(bv);
        let (s2, c2) = s1.overflowing_add(carry);
        a[i] = s2;
        carry = (c1 as u64) + (c2 as u64);
    }
    if carry > 0 {
        a.push(carry);
    }
    big_trim(a);
}

fn big_div_small_rem(b: &mut Big, d: u64) -> u64 {
    let mut rem = 0u64;
    for l in b.iter_mut().rev() {
        let cur = ((rem as u128) << 64) | (*l as u128);
        *l = (cur / d as u128) as u64;
        rem = (cur % d as u128) as u64;
    }
    big_trim(b);
    rem
}

/// floor(b / 2^k), valid when the result fits a u64 (callers ensure b < 2^k * 36).
fn big_shr_small(b: &Big, k: u64) -> u64 {
    let words = (k / 64) as usize;
    let rem = (k % 64) as u32;
    if words >= b.len() {
        return 0;
    }
    let mut v = b[words] >> rem;
    if rem > 0 && words + 1 < b.len() {
        v |= b[words + 1] << (64 - rem);
    }
    v
}

/// b &= (2^k - 1)
fn big_mask_low(b: &mut Big, k: u64) {
    if k == 0 {
        *b = vec![0];
        return;
    }
    let words = (k / 64) as usize;
    let rem = (k % 64) as u32;
    if words >= b.len() {
        return; // b < 2^(64*words) <= 2^k — nothing to mask
    }
    b.truncate(words + 1);
    if rem > 0 {
        b[words] &= (1u64 << rem) - 1;
    } else {
        b.truncate(words);
    }
    big_trim(b);
}

fn big_to_radix(b: &Big, radix: u32) -> String {
    if big_is_zero(b) {
        return "0".to_string();
    }
    let dig = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = b.clone();
    let mut rev = Vec::new();
    while !big_is_zero(&n) {
        let r = big_div_small_rem(&mut n, radix as u64);
        rev.push(dig[r as usize] as char);
    }
    rev.iter().rev().collect()
}

fn big_to_decimal(b: &Big) -> String {
    big_to_radix(b, 10)
}

/// (mantissa, unbiased exponent) such that x = mant * 2^(exp - 52), mant < 2^53.
fn decompose(x: f64) -> (u64, i64) {
    let bits = x.to_bits();
    let mut exp = ((bits >> 52) & 0x7ff) as i64;
    let mut mant = bits & ((1u64 << 52) - 1);
    if exp == 0 {
        exp = 1; // subnormal: no implicit leading 1
    } else {
        mant |= 1u64 << 52;
    }
    (mant, exp - 1023)
}

/// V8 `Double::NextDouble()`: the next representable f64 above `f`
/// (below for negatives), +Infinity for +Infinity.
fn next_double(f: f64) -> f64 {
    let bits = f.to_bits();
    if bits == 0x7ff0_0000_0000_0000 {
        return f; // +Infinity
    }
    let neg = bits >> 63 == 1;
    if neg && bits & 0x000f_ffff_ffff_ffff == 0 {
        return 0.0; // -0.0
    }
    f64::from_bits(if neg { bits - 1 } else { bits + 1 })
}

/// V8 `Double::Exponent()`: biased exponent field minus 1075, -1074 for
/// subnormals. Note the masking must happen BEFORE the shift — `(bits &
/// mask) >> 52`, not `bits & mask >> 52` (Rust precedence would shift first).
fn double_exponent(f: f64) -> i32 {
    let bits = f.to_bits();
    if bits & 0x7ff0_0000_0000_0000 == 0 {
        return -1074;
    }
    let biased = ((bits & 0x7ff0_0000_0000_0000) >> 52) as i32;
    biased - 1075
}

/// V8-exact `Number.prototype.toString(radix)` for radix != 10.
/// Faithful port of V8's `DoubleToRadixCString` (src/numbers/conversions.cc):
/// the fractional digits are computed with f64 arithmetic driven by `delta`
/// (half the distance to the next double), with round-to-even termination and
/// a back-tracing carry; the integer part pads zeros only when the value is
/// >= 2^53, then extracts digits by repeated `%`/`/`. All f64 operations must
/// > stay in f64 — this is not shortest-round-trip, and it intentionally
/// > reproduces V8's IEEE-arithmetic digit counts (e.g. 0.1.toString(16) emits
/// > 14 digits, 1.5.toString(3) ends in "12" after round-to-even).
/// > Verified against Node on 1518 (value, radix) pairs.
pub(crate) fn js_to_string_radix(x: f64, radix: u32) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let radix_f = radix as f64;

    let neg = x < 0.0;
    let v = x.abs();
    let mut integer = v.floor();
    let mut fraction = v - integer;

    // delta = 0.5 * (NextDouble(v) - v), clamped to the minimum denormal delta.
    let mut delta = 0.5 * (next_double(v) - v);
    delta = delta.max(next_double(0.0));

    // Fractional digits (only when the fraction is representable).
    let mut frac: Vec<u8> = Vec::new();
    if fraction >= delta {
        loop {
            fraction *= radix_f;
            delta *= radix_f;
            let digit = fraction as usize;
            frac.push(chars[digit]);
            fraction -= digit as f64;
            // Round to even.
            if (fraction > 0.5 || (fraction == 0.5 && (digit & 1) == 1))
                && fraction + delta > 1.0
            {
                // Back-trace already-written digits in case of carry-over.
                while let Some(&c) = frac.last() {
                    let d = if c > b'9' { c - b'a' + 10 } else { c - b'0' };
                    if d + 1 < radix as u8 {
                        let pos = frac.len() - 1;
                        frac[pos] = chars[(d + 1) as usize];
                        frac.truncate(pos + 1); // digits after the bump are dropped
                        break;
                    }
                    frac.pop(); // digit rolls to 0, carry continues
                }
                if frac.is_empty() {
                    integer += 1.0; // carried all the way to the integer part
                }
                break;
            }
            if fraction < delta {
                break;
            }
            if frac.len() > 4096 {
                break; // safety guard; never reached for finite f64
            }
        }
    }

    // Integer digits. The while loop only fires for values >= 2^53 (V8's
    // Exponent() > 0), padding the top digits with zeros; then extract the
    // remaining digits least-significant-first.
    let mut int_digits: Vec<u8> = Vec::new();
    while double_exponent(integer / radix_f) > 0 {
        integer /= radix_f;
        int_digits.push(b'0');
    }
    loop {
        let remainder = integer % radix_f;
        int_digits.push(chars[remainder as usize]);
        integer = (integer - remainder) / radix_f;
        if integer <= 0.0 {
            break;
        }
    }
    int_digits.reverse(); // emitted least-significant-first

    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.extend(int_digits.iter().map(|&c| c as char));
    if !frac.is_empty() {
        s.push('.');
        s.extend(frac.iter().map(|&c| c as char));
    }
    s
}

/// V8-exact `Number.prototype.toFixed(f)`: rounds the EXACT binary value
/// half-away-from-zero (1.005 -> "1.00", 2.5 -> "3").
pub(crate) fn js_to_fixed(x: f64, f: i64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x.abs() >= 1e21 {
        return js_number_to_string(x);
    }
    if x == 0.0 {
        let mut s = "0".to_string();
        if f > 0 {
            s.push('.');
            for _ in 0..f {
                s.push('0');
            }
        }
        return s;
    }
    let neg = x < 0.0;
    let a = x.abs();
    let (mant, exp) = decompose(a);
    // n = round_half_away(a * 10^f) = round_half_away(mant * 5^f * 2^(f + exp - 52))
    let mut num = big_from_u64(mant);
    for _ in 0..f {
        big_mul_small(&mut num, 5);
    }
    let k = 52 - f - exp;
    if k <= 0 {
        big_shl(&mut num, (-k) as u64);
    } else {
        let mut half = big_from_u64(1);
        big_shl(&mut half, (k - 1) as u64);
        big_add(&mut num, &half);
        big_shr(&mut num, k as u64);
    }
    let digits = big_to_decimal(&num);
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let fl = f as usize;
    if digits.len() <= fl {
        out.push('0');
        if fl > 0 {
            out.push('.');
            for _ in 0..(fl - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        let split = digits.len() - fl;
        out.push_str(&digits[..split]);
        if fl > 0 {
            out.push('.');
            out.push_str(&digits[split..]);
        }
    }
    out
}

/// V8-exact `Number.prototype.toPrecision(p)`.
pub(crate) fn js_to_precision(x: f64, p: i64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if x == 0.0 {
        if p <= 1 {
            return "0".to_string();
        }
        let mut s = "0.".to_string();
        for _ in 1..p {
            s.push('0');
        }
        return s;
    }
    let neg = x < 0.0;
    let a = x.abs();
    let (mant, exp) = decompose(a);
    let shift = exp - 52;
    let mut int_big = big_from_u64(0);
    let mut frac_mant = 0u64;
    let mut k = 0u64;
    if shift >= 0 {
        int_big = big_from_u64(mant);
        big_shl(&mut int_big, shift as u64);
    } else {
        k = (-shift) as u64;
        if k >= 64 {
            frac_mant = mant;
        } else {
            int_big = big_from_u64(mant >> k);
            frac_mant = mant & ((1u64 << k as u32) - 1);
        }
    }
    let int_digits = big_to_decimal(&int_big);
    let int_digits: Vec<u8> = int_digits.bytes().map(|b| b - b'0').collect();
    // exact decimal expansion of the fraction part (terminates: dyadic)
    let mut num_frac = big_from_u64(frac_mant);
    let mut frac: Vec<u8> = Vec::new();
    let mut guard = 0;
    while guard < 2000 {
        if big_is_zero(&num_frac) {
            break;
        }
        big_mul_small(&mut num_frac, 10);
        let d = big_shr_small(&num_frac, k) as u8;
        big_mask_low(&mut num_frac, k);
        frac.push(d);
        guard += 1;
    }
    let mut sig: Vec<u8>; // significant digits (no leading zeros)
    let n: i64;
    if int_digits != [0] {
        sig = int_digits;
        n = sig.len() as i64;
        // append fraction digits (leading zeros included) up to p+1 significant
        let need = (p + 1) as usize;
        sig.extend(frac.iter().take(need.saturating_sub(sig.len())));
    } else {
        let first = frac.iter().position(|&d| d != 0);
        match first {
            Some(idx) => {
                sig = frac[idx..].to_vec();
                n = -(idx as i64);
            }
            None => {
                // all zeros — unreachable (x != 0), but keep it safe
                sig = vec![0];
                n = 0;
            }
        }
        // pad if the expansion terminated before p+1 significant digits
        while (sig.len() as i64) < p + 1 {
            sig.push(0);
        }
    }
    // Round `sig` to p digits, half away from zero (exact digits).
    let mut q: Vec<u8>;
    if sig.len() as i64 <= p {
        q = sig;
        while (q.len() as i64) < p {
            q.push(0);
        }
    } else {
        let keep = &sig[..p as usize];
        let next = sig[p as usize];
        let mut kd = keep.to_vec();
        if next >= 5 {
            let mut i = kd.len();
            while i > 0 {
                i -= 1;
                if kd[i] < 9 {
                    kd[i] += 1;
                    break;
                }
                kd[i] = 0;
            }
            if kd.iter().all(|&d| d == 0) {
                kd.insert(0, 1); // carry past the front: 999... -> 1000...
            }
        }
        q = kd;
    }
    let qd = q.len() as i64;
    let n2 = qd + (n - p);
    // significant digits for display (strip trailing zeros, pad to exactly p)
    let mut s = q.clone();
    while s.len() > 1 && *s.last().unwrap() == 0 {
        s.pop();
    }
    while (s.len() as i64) < p {
        s.push(0);
    }
    let dstr: String = s.iter().map(|&d| (b'0' + d) as char).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    // Exponential boundaries per spec (21.1.3.3): e >= p or e < -6, where
    // e = n2 - 1 is the decimal exponent. So p < n2 (e >= p) or n2 <= -6
    // (e < -6). Note n2 <= -6, not < -6: 1e-7 (n2 = -6) is exponential while
    // 1e-6 (n2 = -5) is fixed. (toFixed's 1e21 rule does NOT apply here —
    // (1e25).toPrecision(30) is fixed "10000000000000000905969664.0000".)
    if p < n2 || n2 <= -6 {
        // exponential: d.ddd e±X
        out.push(dstr.as_bytes()[0] as char);
        if p > 1 {
            out.push('.');
            out.push_str(&dstr[1..]);
        }
        let e = n2 - 1;
        out.push('e');
        if e < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        out.push_str(&e.abs().to_string());
    } else if n2 <= 0 {
        out.push_str("0.");
        for _ in 0..(-n2) {
            out.push('0');
        }
        out.push_str(&dstr);
    } else if (n2 as usize) >= dstr.len() {
        out.push_str(&dstr);
        for _ in 0..(n2 - dstr.len() as i64) {
            out.push('0');
        }
    } else {
        out.push_str(&dstr[..n2 as usize]);
        out.push('.');
        out.push_str(&dstr[n2 as usize..]);
    }
    out
}

pub(crate) fn to_integer_or_infinity(v: &Value) -> f64 {
    let n = v.to_number();
    if n.is_nan() {
        0.0
    } else {
        n.trunc()
    }
}

pub(crate) fn number_prop(obj: &Value, name: &str) -> Value {
    let n = match obj.as_number() {
        Some(n) => n,
        None => obj.as_int().map(|i| i as f64).unwrap_or(f64::NAN),
    };
    match name {
        "toString" => Value::native(Arc::new(move |args, vm| {
            let radix = match args.first() {
                Some(v) if v.is_undefined() => 10.0,
                Some(v) => to_integer_or_infinity(v),
                None => 10.0,
            };
            if !(2.0..=36.0).contains(&radix) {
                vm.throw_exception(Value::string(
                    "RangeError: toString() radix argument must be between 2 and 36".to_string(),
                ));
                return Value::undefined();
            }
            let r = radix as u32;
            if r == 10 {
                Value::string(js_number_to_string(n))
            } else {
                Value::string(js_to_string_radix(n, r))
            }
        })),
        "toFixed" => Value::native(Arc::new(move |args, vm| {
            let f = match args.first() {
                Some(v) if v.is_undefined() => 0.0,
                Some(v) => to_integer_or_infinity(v),
                None => 0.0,
            };
            if !(0.0..=100.0).contains(&f) {
                vm.throw_exception(Value::string(
                    "RangeError: toFixed() digits argument must be between 0 and 100".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(js_to_fixed(n, f as i64))
        })),
        "toPrecision" => Value::native(Arc::new(move |args, vm| {
            let p = match args.first() {
                Some(v) if v.is_undefined() => {
                    return Value::string(js_number_to_string(n));
                }
                Some(v) => to_integer_or_infinity(v),
                None => {
                    return Value::string(js_number_to_string(n));
                }
            };
            if !(1.0..=100.0).contains(&p) {
                vm.throw_exception(Value::string(
                    "RangeError: toPrecision() argument must be between 1 and 100".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(js_to_precision(n, p as i64))
        })),
        _ => Value::undefined(),
    }
}
/// Wrap a finite integral `f64` result as an int (keeping the SMI kind lanes
/// warm), preserving `-0` (JS `Math.floor(-0)` and `Math.round(-0.4)` are
/// `-0`) and passing NaN/±Infinity through as numbers.
pub(crate) fn num_result(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && !(x == 0.0 && x.is_sign_negative()) {
        Value::int(x as i64)
    } else {
        Value::number(x)
    }
}

/// ES parseInt: trim whitespace, optional sign, ToInt32(radix) (NaN/0/
/// undefined → 0, ±Infinity → 0), hex-prefix auto-detection when the radix
/// resolves to 0 or 16, then parse digits in the chosen radix until the first
/// invalid char. No digits → NaN. `"010"` is decimal (10), `"0b101"` → 0
/// (no binary prefix — that's Number()).
pub(crate) fn js_parse_int(v: &Value, radix: &Value) -> Value {
    let owned = to_string_js(v);
    let s = js_trim(&owned);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
        neg = chars[i] == '-';
        i += 1;
    }
    // ToInt32(radix): NaN/±∞ → 0, truncate, fold mod 2^32.
    let r32 = {
        let x = radix.to_number();
        let t = if x.is_nan() || x.is_infinite() {
            0.0
        } else {
            x.trunc()
        };
        let m = t % 4294967296.0;
        let m = if m < 0.0 { m + 4294967296.0 } else { m };
        (m as u32) as i32
    };
    let rest = &chars[i..];
    let hex = rest.len() >= 2
        && rest[0] == '0'
        && (rest[1] == 'x' || rest[1] == 'X');
    let radix = if r32 == 0 {
        if hex {
            i += 2;
            16
        } else {
            10
        }
    } else if r32 == 16 && hex {
        i += 2;
        16
    } else {
        r32
    };
    if !(2..=36).contains(&radix) {
        return Value::number(f64::NAN);
    }
    let mut acc: f64 = 0.0;
    let mut any = false;
    for c in &chars[i..] {
        match c.to_digit(radix as u32) {
            Some(d) => {
                acc = acc * radix as f64 + d as f64;
                any = true;
            }
            None => break,
        }
    }
    if !any {
        return Value::number(f64::NAN);
    }
    Value::number(if neg { -acc } else { acc })
}

/// ES parseFloat: trim whitespace, optional sign, `Infinity` literal, then the
/// longest decimal prefix (mantissa + optional fraction + optional exponent).
/// At least one mantissa digit is required, else NaN. The prefix is parsed
/// with the engine's JS-number parser (handles ".5", "5.", "1.e3").
pub(crate) fn js_parse_float(v: &Value) -> Value {
    let owned = to_string_js(v);
    let s = js_trim(&owned);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
        neg = chars[i] == '-';
        i += 1;
    }
    if chars.len() - i >= 8 && chars[i..i + 8] == ['I', 'n', 'f', 'i', 'n', 'i', 't', 'y'] {
        return Value::number(if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let start = i;
    let mut digits = 0usize;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        let mut j = i + 1;
        let mut edigits = 0usize;
        if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
            j += 1;
        }
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
            edigits += 1;
        }
        if edigits > 0 {
            i = j;
        }
    }
    if digits == 0 {
        return Value::number(f64::NAN);
    }
    let prefix: String = chars[start..i].iter().collect();
    let n = alloy_core::value::js_string_to_number(&prefix);
    Value::number(if neg { -n } else { n })
}

/// Math.random as a native: xorshift64* seeded from time + a counter (the
/// VM thread is the only caller). Must return a *closure* — a bare number
/// would be dispatched as a bytecode entry pointer if called.
pub(crate) fn make_random() -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0);
    Value::native(Arc::new(move |_args, _vm| {
        let seed = if SEED.load(Ordering::Relaxed) == 0 {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15);
            SEED.store(t | 1, Ordering::Relaxed);
            t | 1
        } else {
            SEED.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed)
        };
        let mut x = seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let r = x.wrapping_mul(0x2545F4914F6CDD1D);
        Value::number((r >> 11) as f64 / (1u64 << 53) as f64)
    }))
}

/// `Math` global: floor/ceil/round/abs/sqrt/pow/min/max/random with JS
/// semantics — round half-away-from-zero with `-0` for negatives in
/// `[-0.5, 0)`, min/max return NaN on any NaN argument and track `-0`
/// (min(-0, 0) → -0, max(-0, 0) → 0), min()/max() with no args →
/// +Infinity/-Infinity.
pub(crate) fn make_math_module() -> Value {
    let unary = |f: fn(f64) -> f64| -> Value {
        Value::native(Arc::new(move |args, _vm| {
            let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
            num_result(f(x))
        }))
    };
    let floor = unary(f64::floor);
    let ceil = unary(f64::ceil);
    let abs = unary(f64::abs);
    let sqrt = unary(f64::sqrt);
    let round = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let r = (x + 0.5).floor();
        if r == 0.0 && x < 0.0 {
            Value::number(-0.0)
        } else {
            num_result(r)
        }
    }));
    let pow = Value::native(Arc::new(|args, _vm| {
        let a = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let b = args.get(1).cloned().unwrap_or(Value::undefined()).to_number();
        num_result(a.powf(b))
    }));
    let min = Value::native(Arc::new(|args, _vm| {
        let mut best = f64::INFINITY;
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                return Value::number(f64::NAN);
            }
            if n < best || (n == best && n.is_sign_negative() && !best.is_sign_negative()) {
                best = n;
            }
        }
        num_result(best)
    }));
    let max = Value::native(Arc::new(|args, _vm| {
        let mut best = f64::NEG_INFINITY;
        for a in args {
            let n = a.to_number();
            if n.is_nan() {
                return Value::number(f64::NAN);
            }
            if n > best || (n == best && best.is_sign_negative() && !n.is_sign_negative()) {
                best = n;
            }
        }
        num_result(best)
    }));
    // Extra-unary wrappers that need NaN + zero handling beyond a bare fn.
    let trunc = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        num_result(x.trunc())
    }));
    let sign = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        if x.is_nan() {
            Value::number(f64::NAN)
        } else if x == 0.0 {
            Value::number(x) // preserves -0
        } else if x > 0.0 {
            Value::int(1)
        } else {
            Value::int(-1)
        }
    }));
    let cbrt = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        num_result(x.cbrt())
    }));
    let hypot = Value::native(Arc::new(|args, _vm| {
        let mut acc = 0.0f64;
        for a in args {
            let n = a.to_number();
            acc = acc.hypot(n);
        }
        num_result(acc)
    }));
    let imul = Value::native(Arc::new(|args, _vm| {
        let a = args.first().cloned().unwrap_or(Value::undefined()).to_number() as u32;
        let b = args.get(1).cloned().unwrap_or(Value::undefined()).to_number() as u32;
        Value::int((a.wrapping_mul(b)) as i32 as i64)
    }));
    let clz32 = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number() as u32;
        Value::int(x.leading_zeros() as i64)
    }));
    let fround = Value::native(Arc::new(|args, _vm| {
        let x = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        Value::number(x as f32 as f64)
    }));
    let atan2 = Value::native(Arc::new(|args, _vm| {
        let y = args.first().cloned().unwrap_or(Value::undefined()).to_number();
        let x = args.get(1).cloned().unwrap_or(Value::undefined()).to_number();
        num_result(y.atan2(x))
    }));
    let mut m = HashMap::new();
    // Constants.
    m.insert("E".to_string(), Value::number(std::f64::consts::E));
    m.insert("LN10".to_string(), Value::number(std::f64::consts::LN_10));
    m.insert("LN2".to_string(), Value::number(std::f64::consts::LN_2));
    m.insert("LOG10E".to_string(), Value::number(std::f64::consts::LOG10_E));
    m.insert("LOG2E".to_string(), Value::number(std::f64::consts::LOG2_E));
    m.insert("PI".to_string(), Value::number(std::f64::consts::PI));
    m.insert("SQRT1_2".to_string(), Value::number(std::f64::consts::FRAC_1_SQRT_2));
    m.insert("SQRT2".to_string(), Value::number(std::f64::consts::SQRT_2));
    // Unary f64 functions.
    for (name, f) in [
        ("floor", floor),
        ("ceil", ceil),
        ("abs", abs),
        ("sqrt", sqrt),
        ("sin", unary(f64::sin)),
        ("cos", unary(f64::cos)),
        ("tan", unary(f64::tan)),
        ("asin", unary(f64::asin)),
        ("acos", unary(f64::acos)),
        ("atan", unary(f64::atan)),
        ("log", unary(f64::ln)),
        ("exp", unary(f64::exp)),
        ("log2", unary(f64::log2)),
        ("log10", unary(f64::log10)),
        ("log1p", unary(f64::ln_1p)),
        ("expm1", unary(f64::exp_m1)),
        ("sinh", unary(f64::sinh)),
        ("cosh", unary(f64::cosh)),
        ("tanh", unary(f64::tanh)),
        ("asinh", unary(f64::asinh)),
        ("acosh", unary(f64::acosh)),
        ("atanh", unary(f64::atanh)),
        ("cbrt", cbrt),
        ("trunc", trunc),
        ("sign", sign),
        ("fround", fround),
    ] {
        m.insert(name.to_string(), f);
    }
    m.insert("round".to_string(), round);
    m.insert("pow".to_string(), pow);
    m.insert("min".to_string(), min);
    m.insert("max".to_string(), max);
    m.insert("hypot".to_string(), hypot);
    m.insert("imul".to_string(), imul);
    m.insert("clz32".to_string(), clz32);
    m.insert("atan2".to_string(), atan2);
    m.insert("random".to_string(), make_random());
    Value::object(m)
}

/// `Number` global: parseInt/parseFloat (identical natives to the top-level
/// globals) and isNaN (type-strict — only the NaN number, no coercion).
pub(crate) fn make_number_module() -> Value {
    let parse_int = Value::native(Arc::new(|args, _vm| {
        js_parse_int(
            &args.first().cloned().unwrap_or(Value::undefined()),
            &args.get(1).cloned().unwrap_or(Value::undefined()),
        )
    }));
    let parse_float = Value::native(Arc::new(|args, _vm| {
        js_parse_float(&args.first().cloned().unwrap_or(Value::undefined()))
    }));
    let is_nan = Value::native(Arc::new(|args, _vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        Value::bool(v.is_number() && v.to_number().is_nan())
    }));
    let mut m = HashMap::new();
    m.insert("parseInt".to_string(), parse_int);
    m.insert("parseFloat".to_string(), parse_float);
    m.insert("isNaN".to_string(), is_nan);
    Value::object(m)
}
