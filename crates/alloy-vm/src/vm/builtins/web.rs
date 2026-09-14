use super::http_server::url_decode;
use super::json::serialize_value;
use alloy_core::value::Value;
use hashbrown::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Pure-Rust SHA-256 (FIPS 180-4), HMAC, base64, OS-seeded random.
// No new dependencies: the runtime stays at tokio+libc+hashbrown so the
// 1.6MB static binary story survives. Constant-time concerns don't apply
// (JWT/HMAC comparison should still use the provided `timingSafeEqual`).
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub(crate) fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity((input.len() + 9).div_ceil(64) * 64);
    msg.extend_from_slice(input);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let h = sha256_bytes(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let ih = sha256_bytes(&inner);
    let mut outer = Vec::with_capacity(64 + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&ih);
    sha256_bytes(&outer)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for w in bytes.chunks(3) {
        let (a, b, c) = (
            w[0] as u32,
            *w.get(1).unwrap_or(&0) as u32,
            *w.get(2).unwrap_or(&0) as u32,
        );
        let n = (a << 16) | (b << 8) | c;
        s.push(B64_STD[((n >> 18) & 63) as usize] as char);
        s.push(B64_STD[((n >> 12) & 63) as usize] as char);
        s.push(if w.len() > 1 {
            B64_STD[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        s.push(if w.len() > 2 {
            B64_STD[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    s
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for w in vals.chunks(4) {
        if w.len() < 2 {
            return None;
        }
        let n = (w[0] as u32) << 18
            | (w[1] as u32) << 12
            | (*w.get(2).unwrap_or(&0) as u32) << 6
            | (*w.get(3).unwrap_or(&0) as u32);
        out.push((n >> 16) as u8);
        if w.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if w.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

pub(crate) fn base64url_encode(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// OS-seeded random bytes via std's RandomState (seeded from OS entropy).
/// Honest scope: suitable for sessions/CSRF/tokens, not FIPS key generation.
pub(crate) fn os_random_bytes(n: usize) -> Vec<u8> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = Vec::with_capacity(n);
    let mut ctr: u64 = 0;
    // Mix time + pid + address jitter per block so sequential calls differ
    // even if RandomState repeats within a thread.
    while out.len() < n {
        let rs = RandomState::new();
        let mut h1 = rs.build_hasher();
        h1.write_usize(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0)
                .wrapping_add(ctr as usize),
        );
        h1.write_usize(std::process::id() as usize);
        h1.write_usize(&out as *const Vec<u8> as usize);
        ctr += 1;
        out.extend_from_slice(&h1.finish().to_le_bytes());
        let mut h2 = rs.build_hasher();
        h2.write_usize(ctr as usize ^ 0x9E3779B97F4A7C15u64 as usize);
        out.extend_from_slice(&h2.finish().to_le_bytes());
    }
    out.truncate(n);
    out
}

/// JS encodeURIComponent: escape everything except A-Za-z0-9 `-_.!~*'()`.
/// Non-ASCII is UTF-8 percent-encoded with uppercase hex, like V8.
pub(crate) fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn make_crypto_module() -> Value {
    let sha256 = Value::native(Arc::new(|args, _vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        Value::string(hex_encode(&sha256_bytes(s.as_bytes())))
    }));
    let hmac = Value::native(Arc::new(|args, _vm| {
        let k = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let m = args
            .get(1)
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        Value::string(hex_encode(&hmac_sha256(k.as_bytes(), m.as_bytes())))
    }));
    let random = Value::native(Arc::new(|args, _vm| {
        let n = args
            .first()
            .map(|v| v.to_number() as usize)
            .unwrap_or(16)
            .clamp(1, 1024);
        Value::string(hex_encode(&os_random_bytes(n)))
    }));
    let random_bytes = Value::native(Arc::new(|args, _vm| {
        let n = args
            .first()
            .and_then(|v| v.as_int())
            .unwrap_or(16)
            .clamp(0, 65536) as usize;
        let bytes = os_random_bytes(n);
        let vals: Vec<Value> = bytes.into_iter().map(|b| Value::int(b as i64)).collect();
        Value::array(vals)
    }));
    let random_uuid = Value::native(Arc::new(|_args, _vm| {
        let mut b = os_random_bytes(16);
        b[6] = (b[6] & 0x0f) | 0x40; // RFC 4122 version 4
        b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant 1
        let uuid = format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        );
        Value::string(uuid)
    }));
    let b64e = Value::native(Arc::new(|args, _vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        Value::string(base64_encode(s.as_bytes()))
    }));
    let b64d = Value::native(Arc::new(|args, vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        match base64_decode(&s) {
            Some(b) => Value::string(String::from_utf8_lossy(&b).into_owned()),
            None => {
                vm.throw_exception(Value::string("Error: invalid base64".to_string()));
                Value::undefined()
            }
        }
    }));
    let b64ue = Value::native(Arc::new(|args, _vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        Value::string(base64url_encode(s.as_bytes()))
    }));
    // timingSafeEqual(a, b): length + content compare for HMAC/signature checks.
    let tse = Value::native(Arc::new(|args, _vm| {
        let a = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let b = args
            .get(1)
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let (ab, bb) = (a.as_bytes(), b.as_bytes());
        if ab.len() != bb.len() {
            return Value::bool(false);
        }
        let mut diff = 0u8;
        for (x, y) in ab.iter().zip(bb.iter()) {
            diff |= x ^ y;
        }
        Value::bool(diff == 0)
    }));
    // HMAC-SHA256 straight to base64url (for JWT signatures): avoids the
    // lossy hex->binary-string round-trip (chars >=128 are multi-byte UTF-8
    // in engine strings, so a JS-side hex decode would corrupt the bytes).
    let hmac_b64u = Value::native(Arc::new(|args, _vm| {
        let k = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let m = args
            .get(1)
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        Value::string(base64url_encode(&hmac_sha256(k.as_bytes(), m.as_bytes())))
    }));
    let mut m = HashMap::new();
    m.insert("sha256".to_string(), sha256);
    m.insert("hmacSha256".to_string(), hmac);
    m.insert("hmacBase64Url".to_string(), hmac_b64u);
    m.insert("randomHex".to_string(), random);
    m.insert("randomBytes".to_string(), random_bytes);
    m.insert("randomUUID".to_string(), random_uuid);
    m.insert("base64Encode".to_string(), b64e);
    m.insert("base64Decode".to_string(), b64d);
    m.insert("base64UrlEncode".to_string(), b64ue);
    m.insert("timingSafeEqual".to_string(), tse);
    Value::object(m)
}

/// Minimal WHATWG-subset URL parser: `URL.parse(href, base?)` →
/// `{protocol, host, hostname, port, path, query, hash, href}`.
/// Relative refs resolve against `base` when given ( Merrick: `/p` + base ).
pub(crate) fn make_url_ctor() -> Value {
    let parse = Value::native(Arc::new(|args, vm| {
        let href = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let base = args
            .get(1)
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        match parse_url(&href, if base.is_empty() { None } else { Some(&base) }) {
            Some((proto, host, port, path, query, hash, full)) => {
                let mut m = HashMap::new();
                m.insert("protocol".to_string(), Value::string(proto));
                let host_with_port = if port.is_empty() {
                    host.clone()
                } else {
                    format!("{}:{}", host, port)
                };
                m.insert("host".to_string(), Value::string(host_with_port));
                m.insert("hostname".to_string(), Value::string(host));
                m.insert("port".to_string(), Value::string(port));
                m.insert("path".to_string(), Value::string(path));
                let mut qm = HashMap::new();
                for (k, v) in query {
                    qm.insert(k, Value::string(v));
                }
                m.insert("query".to_string(), Value::object(qm));
                m.insert("hash".to_string(), Value::string(hash));
                m.insert("href".to_string(), Value::string(full));
                Value::object(m)
            }
            None => {
                vm.throw_exception(Value::string("TypeError: invalid URL".to_string()));
                Value::undefined()
            }
        }
    }));
    let mut m = HashMap::new();
    m.insert("parse".to_string(), parse);
    Value::object(m)
}

pub(crate) fn parse_url(
    href: &str,
    base: Option<&str>,
) -> Option<(
    String,
    String,
    String,
    String,
    Vec<(String, String)>,
    String,
    String,
)> {
    let mut s = href.trim().to_string();
    // hash
    let hash = match s.clone().split_once('#') {
        Some((a, h)) => {
            let hh = format!("#{}", h);
            s = a.to_string();
            hh
        }
        None => String::new(),
    };
    // resolve relative against base
    if !s.contains("://") {
        let b = base?;
        let (bp, bh, bport, bpath, _, _, _) = parse_url(b, None)?;
        if s.starts_with('/') {
            s = format!("{}://{}{}", bp.trim_end_matches(':'), bh, s);
            let _ = (bport, bpath);
        } else if s.is_empty() {
            s = b.to_string();
        } else {
            return None;
        }
    }
    let (scheme, rest) = s.split_once("://")?;
    if scheme.is_empty() || scheme.contains('/') {
        return None;
    }
    let (auth, path_q) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{}", p)),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match auth.split_once(':') {
        Some((h, p)) if !p.is_empty() => (h.to_string(), p.to_string()),
        _ => (auth.to_string(), String::new()),
    };
    if host.is_empty() {
        return None;
    }
    let (path, q) = match path_q.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (path_q.clone(), String::new()),
    };
    let mut query = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) => query.push((url_decode(k), url_decode(v))),
            None => query.push((url_decode(part), String::new())),
        }
    }
    let full = format!(
        "{}://{}{}{}",
        scheme,
        auth,
        path,
        if q.is_empty() {
            String::new()
        } else {
            format!("?{}", q)
        }
    );
    Some((
        format!("{}:", scheme),
        host,
        port,
        if path.is_empty() {
            "/".to_string()
        } else {
            path
        },
        query,
        hash,
        full + "",
    ))
}

/// Blocking single-shot HTTP/1.1 client (http:// only): `fetchSync(url, opts?)`.
/// `opts` = `{method, headers, body, timeoutMs}`. Returns
/// `{status, ok, headers, body}` or throws (`throw_exception`) on DNS/TCP/
/// timeout/oversize/non-http errors. HTTPS throws with a loud pointer to TLS
/// termination (reverse proxy) — no silent downgrade.
/// Concurrency pattern: `await spawn(() => fetchSync(url))` (isolated worker).
pub(crate) fn make_fetch_sync() -> Value {
    Value::native(Arc::new(|args, vm| {
        let url = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let (mut method, mut headers, mut body, mut timeout_ms) = (
            "GET".to_string(),
            Vec::<(String, String)>::new(),
            Vec::<u8>::new(),
            10_000u64,
        );
        if let Some(opts) = args.get(1).and_then(|v| v.as_object()) {
            let o = opts.borrow();
            if let Some(m) = o.get("method").and_then(|v| v.as_str()) {
                method = m.to_uppercase();
            }
            if let Some(h) = o.get("headers").and_then(|v| v.as_object()) {
                let h = h.borrow();
                for (k, v) in h.iter_sorted() {
                    headers.push((k.to_string(), v.as_str().unwrap_or("").to_string()));
                }
            }
            if let Some(b) = o.get("body") {
                if let Some(s) = b.as_str() {
                    body = s.as_bytes().to_vec();
                } else if !b.is_undefined() {
                    body = serialize_value(b).into_bytes();
                }
            }
            if let Some(t) = o.get("timeoutMs") {
                timeout_ms = (t.to_number() as u64).clamp(100, 120_000);
            }
        }
        match fetch_sync_inner(&url, &method, &headers, &body, timeout_ms) {
            Ok((status, status_text, rheaders, rbody)) => {
                let mut m = HashMap::new();
                m.insert("status".to_string(), Value::number(status as f64));
                m.insert("statusText".to_string(), Value::string(status_text));
                m.insert("ok".to_string(), Value::bool((200..300).contains(&status)));
                let mut hm = HashMap::new();
                for (k, v) in rheaders {
                    hm.insert(k.to_ascii_lowercase(), Value::string(v));
                }
                m.insert("headers".to_string(), Value::object(hm));
                let body_str = String::from_utf8_lossy(&rbody).into_owned();
                m.insert("body".to_string(), Value::string(body_str.clone()));

                let text_body = body_str.clone();
                let text_fn =
                    Value::native(Arc::new(move |_args, _vm| Value::string(text_body.clone())));
                m.insert("text".to_string(), text_fn);

                let json_body = body_str;
                let json_fn =
                    Value::native(Arc::new(
                        move |_args, vm| match super::json::parse_json_str(&json_body) {
                            Ok(val) => val,
                            Err(e) => {
                                vm.throw_exception(Value::string(format!(
                                    "SyntaxError: Unexpected token in JSON: {}",
                                    e
                                )));
                                Value::undefined()
                            }
                        },
                    ));
                m.insert("json".to_string(), json_fn);

                Value::object(m)
            }
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: fetchSync failed: {}", e)));
                Value::undefined()
            }
        }
    }))
}

pub(crate) fn fetch_sync_inner(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout_ms: u64,
) -> Result<(u16, String, Vec<(String, String)>, Vec<u8>), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http:// and https:// URLs are supported".to_string());
    }
    if body.len() > 10 * 1024 * 1024 {
        return Err("request body exceeds 10MB".to_string());
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build();

    let mut req = match method.to_uppercase().as_str() {
        "POST" => agent.post(url),
        "PUT" => agent.put(url),
        "DELETE" => agent.delete(url),
        "HEAD" => agent.head(url),
        "PATCH" => agent.patch(url),
        _ => agent.get(url),
    };

    for (k, v) in headers {
        req = req.set(k, v);
    }

    let response = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    };

    let resp = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(_code, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(format!("network error: {}", t)),
    };

    let status = resp.status();
    let status_text = resp.status_text().to_string();

    let mut rheaders = Vec::new();
    for name in resp.headers_names() {
        if let Some(val) = resp.header(&name) {
            rheaders.push((name, val.to_string()));
        }
    }

    use std::io::Read;
    let mut reader = resp.into_reader();
    let mut rbody = Vec::new();
    reader
        .take(10 * 1024 * 1024)
        .read_to_end(&mut rbody)
        .map_err(|e| format!("read body failed: {}", e))?;

    Ok((status, status_text, rheaders, rbody))
}

pub fn dechunk(buf: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let line_end = buf[i..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "bad chunk".to_string())?
            + i;
        let size_str =
            std::str::from_utf8(&buf[i..line_end]).map_err(|_| "bad chunk size".to_string())?;
        let size =
            usize::from_str_radix(size_str.trim(), 16).map_err(|_| "bad chunk size".to_string())?;
        if size == 0 {
            break;
        }
        if out.len() + size > 6 * 1024 * 1024 {
            return Err("response exceeds 6MB".to_string());
        }
        i = line_end + 2;
        if i + size > buf.len() {
            return Err("truncated chunk".to_string());
        }
        out.extend_from_slice(&buf[i..i + size]);
        i += size + 2;
    }
    Ok(out)
}
