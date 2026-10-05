use super::json::serialize_value;
use alloy_core::value::{PromiseState, PromiseStatus, Value, VmHost};
use hashbrown::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

pub(crate) fn make_http_module() -> Value {
    let create_server = Value::native(Arc::new(|args, _vm| {
        let handler = args.first().cloned().unwrap_or(Value::undefined());
        let listen = Value::native(Arc::new(move |listen_args, vm| {
            let port = listen_args.first().map(|v| v.to_number()).unwrap_or(0.0) as u16;
            serve_http(vm, &handler, port);
            Value::undefined()
        }));
        let mut srv = HashMap::new();
        srv.insert("listen".to_string(), listen);
        Value::object(srv)
    }));
    let mut m = HashMap::new();
    m.insert("createServer".to_string(), create_server);
    Value::object(m)
}

pub(crate) fn parse_http_request(text: &str) -> (String, String, String) {
    let (m, u, b, _) = parse_http_request_full(text);
    (m, u, b)
}

/// Decode HTTP/1.1 chunked transfer-encoded data.
/// Returns `Some((decoded_payload, total_consumed_bytes))` if a full chunked body
/// has arrived (terminated by `0\r\n\r\n` or `0\r\n[trailers]\r\n`).
/// Returns `None` if the chunked stream is still incomplete (needs more data).
pub fn decode_chunked_body(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut decoded = Vec::new();
    let mut offset = 0;

    loop {
        let rem = &buf[offset..];
        let crlf = rem.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&rem[..crlf]).ok()?.trim();
        let hex_part = size_str.split(';').next()?.trim();
        let chunk_size = usize::from_str_radix(hex_part, 16).ok()?;

        let chunk_data_start = offset + crlf + 2;

        if chunk_size == 0 {
            // Last chunk. Look for trailer end "\r\n\r\n" or immediate "\r\n"
            let rem_trailers = &buf[chunk_data_start..];
            if rem_trailers.starts_with(b"\r\n") {
                return Some((decoded, chunk_data_start + 2));
            }
            let end_trailers = rem_trailers.windows(4).position(|w| w == b"\r\n\r\n")?;
            return Some((decoded, chunk_data_start + end_trailers + 4));
        }

        let chunk_data_end = chunk_data_start + chunk_size;
        // Need chunk data + trailing "\r\n"
        if buf.len() < chunk_data_end + 2 {
            return None; // incomplete chunk
        }
        if &buf[chunk_data_end..chunk_data_end + 2] != b"\r\n" {
            // Malformed chunk framing
            return None;
        }

        decoded.extend_from_slice(&buf[chunk_data_start..chunk_data_end]);
        offset = chunk_data_end + 2;
    }
}

/// Full parse: method, url (with query), body string, headers (original case).
pub fn parse_http_request_full(text: &str) -> (String, String, String, Vec<(String, String)>) {
    let mut method = "GET".to_string();
    let mut path = "/".to_string();
    let mut headers = Vec::new();
    let (head, raw_body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => (text, ""),
    };
    let mut lines = head.lines();
    if let Some(first) = lines.next() {
        let mut parts = first.split_whitespace();
        if let Some(m) = parts.next() {
            method = m.to_string();
        }
        if let Some(p) = parts.next() {
            path = p.to_string();
        }
    }
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if !k.is_empty() {
                headers.push((k.to_string(), v.trim().to_string()));
            }
        }
    }
    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    let body = if is_chunked {
        decode_chunked_body(raw_body.as_bytes())
            .map(|(d, _)| String::from_utf8_lossy(&d).into_owned())
            .unwrap_or_else(|| raw_body.to_string())
    } else {
        raw_body.to_string()
    };
    (method, path, body, headers)
}

/// Extract HTTP protocol version ("HTTP/1.1", "HTTP/1.0", etc.) from the first line.
pub(crate) fn parse_http_version(text: &str) -> &str {
    if let Some(first_line) = text.lines().next() {
        if let Some(v) = first_line.split_whitespace().nth(2) {
            return v;
        }
    }
    "HTTP/1.1"
}

/// Check if connection should remain open across requests (HTTP/1.1 persistent connection).
pub(crate) fn should_keep_alive(headers: &[(String, String)], text: &str) -> bool {
    let version = parse_http_version(text);
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("connection") {
            if v.eq_ignore_ascii_case("close") {
                return false;
            }
            if v.eq_ignore_ascii_case("keep-alive") {
                return true;
            }
        }
    }
    !version.starts_with("HTTP/1.0")
}

/// One in-flight HTTP connection: reading the request, running its handler,
/// or done. Reads are non-blocking and incremental, so a client that connects
/// and stalls mid-request never blocks the loop — it just sits here until it
/// sends, closes, or times out.
struct PendingRequest {
    stream: std::net::TcpStream,
    /// Accumulated request bytes while still reading.
    buf: Vec<u8>,
    /// True once the handler was invoked (request fully read).
    started: bool,
    /// Response slots; Some once the handler runs.
    body: Option<ResSlots>,
    /// The handler's own promise when it suspended (None for sync handlers
    /// and while still reading).
    done: Option<Value>,
    /// When the connection was accepted; stalled reads are dropped after this
    /// + REQUEST_READ_TIMEOUT so they can't leak connections.
    accepted_at: std::time::Instant,
    /// Whether this connection should be kept open for subsequent requests.
    keep_alive: bool,
    /// Whether response headers were already sent to client (for chunked streams).
    headers_sent: bool,
}

/// Compute total byte length of the first complete HTTP request in `buf`.
/// Returns `Some(len)` if headers and full body (Content-Length or chunked) have arrived.
/// Returns `None` if the request is incomplete.
pub fn find_request_len(buf: &[u8]) -> Option<usize> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let body_start = header_end + 4;
    let headers_str = String::from_utf8_lossy(&buf[..header_end]);
    let is_chunked = headers_str.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });

    if is_chunked {
        let (_, consumed) = decode_chunked_body(&buf[body_start..])?;
        Some(body_start + consumed)
    } else {
        let content_len = headers_str
            .lines()
            .find_map(|l| {
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if buf.len() >= body_start + content_len {
            Some(body_start + content_len)
        } else {
            None
        }
    }
}

/// A request is complete once its header block ("\r\n\r\n") has arrived and,
/// for requests declaring a body, all Content-Length or chunked bytes are in.
pub(crate) fn request_complete(buf: &[u8]) -> bool {
    find_request_len(buf).is_some()
}

/// Percent-decode a URL component (`+` → space, `%XX` → byte). Malformed
/// sequences pass through literally rather than failing the request.
pub(crate) fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
pub(crate) fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Split `/path?a=1&b=x` into path + decoded query pairs.
pub(crate) fn split_path_query(url: &str) -> (String, Vec<(String, String)>) {
    let (path, q) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    };
    let mut pairs = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) => pairs.push((url_decode(k), url_decode(v))),
            None => pairs.push((url_decode(part), String::new())),
        }
    }
    (path.to_string(), pairs)
}

/// Parse `Cookie: a=1; b=x` into pairs (names trimmed, values unquoted).
pub(crate) fn parse_cookies(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            let v = v.trim().trim_matches('"');
            Some((k.to_string(), url_decode(v)))
        })
        .collect()
}

pub(crate) fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// Response slots shared with the `res` natives below.
#[derive(Clone, Default)]
struct ResSlots {
    body: Arc<Mutex<Option<Vec<u8>>>>,
    status: Arc<Mutex<u16>>,
    headers: Arc<Mutex<Vec<(String, String)>>>,
    content_type: Arc<Mutex<Option<String>>>,
    stream_chunks: Arc<Mutex<Vec<Vec<u8>>>>,
    is_chunked: Arc<Mutex<bool>>,
    ended: Arc<Mutex<bool>>,
}

/// Parse a complete request, invoke the handler, and return the response slots
/// plus the handler's promise (None for sync handlers).
fn start_handler(
    vm: &mut dyn VmHost,
    handler: &Value,
    req_text: &str,
) -> (ResSlots, Option<Value>) {
    let (method, url, body, headers) = parse_http_request_full(req_text);
    let slots = ResSlots::default();
    // res.send(obj|string): JSON for objects (legacy), raw bytes for strings.
    let s_send = slots.clone();
    let send = Value::native(Arc::new(move |args, _vm| {
        let mut slot = s_send.body.lock().unwrap();
        *slot = args.first().map(|v| {
            if let Some(s) = v.as_str() {
                s.as_bytes().to_vec()
            } else {
                serialize_value(v).into_bytes()
            }
        });
        *s_send.ended.lock().unwrap() = true;
        Value::undefined()
    }));
    // res.json(obj): explicit JSON.
    let s_json = slots.clone();
    let json = Value::native(Arc::new(move |args, _vm| {
        let mut slot = s_json.body.lock().unwrap();
        *slot = args.first().map(|v| serialize_value(v).into_bytes());
        let mut ct = s_json.content_type.lock().unwrap();
        if ct.is_none() {
            *ct = Some("application/json".to_string());
        }
        *s_json.ended.lock().unwrap() = true;
        Value::undefined()
    }));
    // res.text(s) / res.html(s): string bodies with content type.
    let s_text = slots.clone();
    let text = Value::native(Arc::new(move |args, _vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        *s_text.body.lock().unwrap() = Some(s.into_bytes());
        let mut ct = s_text.content_type.lock().unwrap();
        if ct.is_none() {
            *ct = Some("text/plain; charset=utf-8".to_string());
        }
        *s_text.ended.lock().unwrap() = true;
        Value::undefined()
    }));
    let s_html = slots.clone();
    let html = Value::native(Arc::new(move |args, _vm| {
        let s = args
            .first()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        *s_html.body.lock().unwrap() = Some(s.into_bytes());
        *s_html.content_type.lock().unwrap() = Some("text/html; charset=utf-8".to_string());
        *s_html.ended.lock().unwrap() = true;
        Value::undefined()
    }));
    let res_holder: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    // res.status(code): override the status (default 200).
    let s_status = slots.clone();
    let r_status = res_holder.clone();
    let status = Value::native(Arc::new(move |args, _vm| {
        let code = args
            .first()
            .map(|v| v.to_number() as u16)
            .unwrap_or(200)
            .clamp(100, 599);
        *s_status.status.lock().unwrap() = code;
        r_status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Value::undefined())
    }));
    // res.set(name, value): extra response header. `Content-Type` replaces
    // the content-type slot (so `res.set("Content-Type", ...)` + `res.text`
    // never emits duplicate Content-Type headers).
    let s_set = slots.clone();
    let r_set = res_holder.clone();
    let set = Value::native(Arc::new(move |args, _vm| {
        let name = args
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let val = args
            .get(1)
            .map(|v| v.as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        if !name.is_empty() {
            if name.eq_ignore_ascii_case("content-type") {
                *s_set.content_type.lock().unwrap() = Some(val);
            } else {
                s_set.headers.lock().unwrap().push((name, val));
            }
        }
        r_set.lock().unwrap().clone().unwrap_or(Value::undefined())
    }));
    // res.write(chunk): stream a chunk using HTTP/1.1 chunked transfer encoding.
    let s_write = slots.clone();
    let r_write = res_holder.clone();
    let write = Value::native(Arc::new(move |args, _vm| {
        *s_write.is_chunked.lock().unwrap() = true;
        if let Some(chunk_val) = args.first() {
            let chunk = if let Some(s) = chunk_val.as_str() {
                s.as_bytes().to_vec()
            } else {
                serialize_value(chunk_val).into_bytes()
            };
            if !chunk.is_empty() {
                s_write.stream_chunks.lock().unwrap().push(chunk);
            }
        }
        r_write.lock().unwrap().clone().unwrap_or(Value::bool(true))
    }));
    // res.end([chunk]): complete the response.
    let s_end = slots.clone();
    let r_end = res_holder.clone();
    let end = Value::native(Arc::new(move |args, _vm| {
        if let Some(chunk_val) = args.first() {
            if !chunk_val.is_undefined() && !chunk_val.is_null() {
                let chunk = if let Some(s) = chunk_val.as_str() {
                    s.as_bytes().to_vec()
                } else {
                    serialize_value(chunk_val).into_bytes()
                };
                let is_chunked = *s_end.is_chunked.lock().unwrap();
                if is_chunked {
                    if !chunk.is_empty() {
                        s_end.stream_chunks.lock().unwrap().push(chunk);
                    }
                } else {
                    *s_end.body.lock().unwrap() = Some(chunk);
                }
            }
        }
        *s_end.ended.lock().unwrap() = true;
        r_end.lock().unwrap().clone().unwrap_or(Value::undefined())
    }));
    let mut res = HashMap::new();
    res.insert("send".to_string(), send);
    res.insert("json".to_string(), json);
    res.insert("text".to_string(), text);
    res.insert("html".to_string(), html);
    res.insert("status".to_string(), status);
    res.insert("set".to_string(), set);
    res.insert("write".to_string(), write);
    res.insert("end".to_string(), end);
    let res_val = Value::object(res);
    *res_holder.lock().unwrap() = Some(res_val.clone());
    let mut req = HashMap::new();
    req.insert("method".to_string(), Value::string(method));
    req.insert("url".to_string(), Value::string(url.clone()));
    req.insert("body".to_string(), Value::string(body));
    let (path_only, query) = split_path_query(&url);
    req.insert("path".to_string(), Value::string(path_only));
    let mut qmap = HashMap::new();
    for (k, v) in &query {
        qmap.insert(k.clone(), Value::string(v.clone()));
    }
    req.insert("query".to_string(), Value::object(qmap));
    let mut hmap = HashMap::new();
    let mut cookie_hdr = String::new();
    for (k, v) in &headers {
        hmap.insert(k.to_ascii_lowercase(), Value::string(v.clone()));
        if k.eq_ignore_ascii_case("cookie") {
            cookie_hdr = v.clone();
        }
    }
    req.insert("headers".to_string(), Value::object(hmap));
    let mut cmap = HashMap::new();
    for (k, v) in parse_cookies(&cookie_hdr) {
        cmap.insert(k, Value::string(v));
    }
    req.insert("cookies".to_string(), Value::object(cmap));
    // Trust proxy headers when present (TLS-terminating reverse proxy pattern).
    if let Some((_, proto)) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-proto"))
    {
        req.insert("protocol".to_string(), Value::string(proto.clone()));
    } else {
        req.insert("protocol".to_string(), Value::string("http".to_string()));
    }
    if let Some((_, ip)) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-for"))
    {
        let first = ip.split(',').next().unwrap_or("").trim().to_string();
        req.insert("ip".to_string(), Value::string(first));
    }
    let result = vm.call_value(handler, &[Value::object(req), res_val]);
    if let Some(err) = vm.take_uncaught_exception() {
        // A synchronous throw inside the handler: surface it as a rejected
        // handler promise so the serve loop responds 500, and clear the
        // uncaught flag — the response *is* the handling, so the VM must not
        // treat it as an uncaught top-level throw that aborts the program.
        let wake = vm.wake_handle();
        let done = Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Rejected(err),
            continuations: Vec::new(),
            owner: wake,
        })));
        return (slots, Some(done));
    }
    let done = result.as_promise().map(|_| result.clone());
    (slots, done)
}

pub(crate) fn write_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    write_response_full(stream, 200, None, &[], body.as_bytes(), false);
    let _ = status;
}

pub(crate) fn write_response_full(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: Option<&str>,
    extra: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) {
    let reason = reason_for(status);
    let ct = content_type.unwrap_or("application/json");
    let conn_header = if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}{}",
        status,
        reason,
        ct,
        body.len(),
        conn_header,
        if keep_alive {
            "Keep-Alive: timeout=15, max=1000\r\n"
        } else {
            ""
        }
    );
    for (k, v) in extra {
        // CRLF injection guard: header names/values must be single-line.
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            continue;
        }
        if k.eq_ignore_ascii_case("content-type")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

pub(crate) fn write_chunked_response_headers(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: Option<&str>,
    extra: &[(String, String)],
    keep_alive: bool,
) {
    let reason = reason_for(status);
    let ct = content_type.unwrap_or("text/plain; charset=utf-8");
    let conn_header = if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n{}{}",
        status,
        reason,
        ct,
        conn_header,
        if keep_alive {
            "Keep-Alive: timeout=15, max=1000\r\n"
        } else {
            ""
        }
    );
    for (k, v) in extra {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            continue;
        }
        if k.eq_ignore_ascii_case("content-type")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

pub(crate) fn write_chunk(stream: &mut std::net::TcpStream, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    let header = format!("{:X}\r\n", chunk.len());
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(chunk);
    let _ = stream.write_all(b"\r\n");
    let _ = stream.flush();
}

pub(crate) fn finish_chunked_response(stream: &mut std::net::TcpStream) {
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

/// Bind the HTTP listener (non-blocking accepts) and return it plus the
/// actual port, so hosts and tests can discover the port before serving.
pub fn bind_server(port: u16) -> Result<(std::net::TcpListener, u16), String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("alloy http bind error: {}", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("alloy http nonblocking error: {}", e))?;
    let actual = listener.local_addr().map_err(|e| e.to_string())?.port();
    println!("alloy http listening on 127.0.0.1:{}", actual);
    Ok((listener, actual))
}

pub(crate) fn serve_http(vm: &mut dyn VmHost, handler: &Value, port: u16) {
    match bind_server(port) {
        Ok((listener, _)) => {
            // The production server lives for the process lifetime, so this
            // flag is never set — it exists so a host (or a test) can stop
            // the loop and let the VM drop cleanly: Vm::drop reaps the
            // python sidecar children (OS processes that would otherwise
            // orphan) and removes the shared-segment file immediately.
            let stop = std::sync::atomic::AtomicBool::new(false);
            serve_loop(vm, handler, listener, &stop);
        }
        Err(e) => eprintln!("{}", e),
    }
}

/// Concurrent request loop: accept everything queued, read requests
/// incrementally (non-blocking), start each handler, pump python completions
/// + microtasks, and write responses for handlers whose promise settled. A
/// slow handler's python call runs on its own worker (same-file calls spread
/// across the file's pool children) while later requests are accepted and
/// started, so no request stalls another; a client that connects and stalls
/// mid-request is parked, never blocks the loop, and times out if it never
/// finishes.
pub fn serve_loop(
    vm: &mut dyn VmHost,
    handler: &Value,
    listener: std::net::TcpListener,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut pending: Vec<PendingRequest> = Vec::new();
    loop {
        // A host asked us to stop: exit so the owning VM drops. That runs
        // Vm::drop, which reaps the VM's python sidecar children — real OS
        // processes that the arena heap cannot reclaim and that a detached
        // serve thread would orphan — and removes its shared-segment file
        // immediately (a leaked file would otherwise wait for the next
        // startup sweep).
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // 1. Accept everything currently queued. The listener is non-blocking,
        //    so a handler awaiting python never stops new connections; the
        //    accepted streams stay non-blocking for the incremental reads.
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    pending.push(PendingRequest {
                        stream,
                        buf: Vec::with_capacity(512),
                        started: false,
                        body: None,
                        done: None,
                        accepted_at: std::time::Instant::now(),
                        keep_alive: true,
                        headers_sent: false,
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        // 2. Read available bytes on every unstarted connection; start the
        //    handler once the request is complete. EOF, errors, and stalled
        //    connections (30s) are dropped without blocking anything.
        let mut i = 0;
        while i < pending.len() {
            if pending[i].started {
                // Connection currently executing a request: read any incoming pipelined data non-blocking
                let mut chunk = [0u8; 4096];
                match pending[i].stream.read(&mut chunk) {
                    Ok(0) => {}
                    Ok(n) => pending[i].buf.extend_from_slice(&chunk[..n]),
                    Err(_) => {}
                }
                i += 1;
                continue;
            }
            let timeout_secs = if pending[i].buf.is_empty() { 15 } else { 30 };
            if pending[i].accepted_at.elapsed() > std::time::Duration::from_secs(timeout_secs) {
                pending.remove(i);
                continue;
            }

            // Check if buf already has a complete request (pipelined from earlier read)
            if let Some(req_len) = find_request_len(&pending[i].buf) {
                let req_bytes = pending[i].buf[..req_len].to_vec();
                pending[i].buf.drain(..req_len);
                let req_text = String::from_utf8_lossy(&req_bytes).to_string();
                let (_, _, _, headers) = parse_http_request_full(&req_text);
                let keep_alive = should_keep_alive(&headers, &req_text);
                let (body, done) = start_handler(vm, handler, &req_text);
                pending[i].started = true;
                pending[i].body = Some(body);
                pending[i].done = done;
                pending[i].keep_alive = keep_alive;
                pending[i].headers_sent = false;
                i += 1;
                continue;
            }

            let mut chunk = [0u8; 4096];
            match pending[i].stream.read(&mut chunk) {
                Ok(0) => {
                    // Client closed connection (normal keep-alive EOF or disconnect)
                    pending.remove(i);
                    continue;
                }
                Ok(n) => pending[i].buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    i += 1;
                    continue;
                }
                Err(_) => {
                    pending.remove(i);
                    continue;
                }
            }

            if let Some(req_len) = find_request_len(&pending[i].buf) {
                let req_bytes = pending[i].buf[..req_len].to_vec();
                pending[i].buf.drain(..req_len);
                let req_text = String::from_utf8_lossy(&req_bytes).to_string();
                let (_, _, _, headers) = parse_http_request_full(&req_text);
                let keep_alive = should_keep_alive(&headers, &req_text);
                let (body, done) = start_handler(vm, handler, &req_text);
                pending[i].started = true;
                pending[i].body = Some(body);
                pending[i].done = done;
                pending[i].keep_alive = keep_alive;
                pending[i].headers_sent = false;
                i += 1;
            } else {
                i += 1;
            }
        }
        // 3. One non-blocking pump: settle python completions, run microtasks
        //    (resuming handlers that were awaiting python).
        vm.pump_async();
        // 4. Write responses for every settled or streaming request
        let mut i = 0;
        let mut completed = false;
        while i < pending.len() {
            if !pending[i].started {
                i += 1;
                continue;
            }

            let slots = match &pending[i].body {
                Some(s) => s.clone(),
                None => {
                    i += 1;
                    continue;
                }
            };

            let is_chunked = *slots.is_chunked.lock().unwrap();

            // Check if handler promise has settled
            let outcome = match &pending[i].done {
                Some(p) => match p.as_promise() {
                    Some(pr) => {
                        let st = pr.lock().unwrap_or_else(|g| g.into_inner());
                        match &st.status {
                            PromiseStatus::Fulfilled(_) => Some(None),
                            PromiseStatus::Rejected(v) => Some(Some(v.clone())),
                            PromiseStatus::Pending => None,
                        }
                    }
                    None => None,
                },
                None => Some(None),
            };

            let is_ended = *slots.ended.lock().unwrap();
            let handler_ended = is_ended || outcome.is_some();

            if is_chunked {
                // Streaming chunked transfer
                if !pending[i].headers_sent {
                    let status = *slots.status.lock().unwrap();
                    let status = if status == 0 { 200 } else { status };
                    let ct = slots.content_type.lock().unwrap().clone();
                    let extra = slots.headers.lock().unwrap().clone();
                    let close_hdr = extra.iter().any(|(k, v)| {
                        k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
                    });
                    if close_hdr {
                        pending[i].keep_alive = false;
                    }
                    let keep_alive = pending[i].keep_alive;
                    write_chunked_response_headers(
                        &mut pending[i].stream,
                        status,
                        ct.as_deref(),
                        &extra,
                        keep_alive,
                    );
                    pending[i].headers_sent = true;
                }

                // Drain any available chunks
                let chunks: Vec<Vec<u8>> = {
                    let mut lock = slots.stream_chunks.lock().unwrap();
                    std::mem::take(&mut *lock)
                };
                for chunk in chunks {
                    write_chunk(&mut pending[i].stream, &chunk);
                }

                if handler_ended {
                    finish_chunked_response(&mut pending[i].stream);
                    completed = true;
                    if pending[i].keep_alive {
                        pending[i].started = false;
                        pending[i].body = None;
                        pending[i].done = None;
                        pending[i].accepted_at = std::time::Instant::now();
                        pending[i].headers_sent = false;
                        i += 1;
                    } else {
                        pending.remove(i);
                    }
                } else {
                    i += 1;
                }
            } else {
                // Non-chunked response: wait until handler settled or ended
                let ready = outcome.is_some() || is_ended;
                if ready {
                    let mut pr = pending.remove(i);
                    completed = true;
                    let extra = slots.headers.lock().unwrap().clone();
                    let close_hdr = extra.iter().any(|(k, v)| {
                        k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")
                    });
                    if close_hdr {
                        pr.keep_alive = false;
                    }
                    match outcome.flatten() {
                        Some(err_val) => {
                            let body = format!("{{\"error\": {}}}", serialize_value(&err_val));
                            write_response_full(
                                &mut pr.stream,
                                500,
                                Some("application/json"),
                                &[],
                                body.as_bytes(),
                                pr.keep_alive,
                            );
                        }
                        None => {
                            let body = slots
                                .body
                                .lock()
                                .unwrap()
                                .clone()
                                .unwrap_or_else(|| b"ok".to_vec());
                            let status = *slots.status.lock().unwrap();
                            let status = if status == 0 { 200 } else { status };
                            let ct = slots.content_type.lock().unwrap().clone();
                            write_response_full(
                                &mut pr.stream,
                                status,
                                ct.as_deref(),
                                &extra,
                                &body,
                                pr.keep_alive,
                            );
                        }
                    }
                    if pr.keep_alive {
                        pr.started = false;
                        pr.body = None;
                        pr.done = None;
                        pr.accepted_at = std::time::Instant::now();
                        pr.headers_sent = false;
                        pending.insert(i, pr);
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        }
        // 5. Per-request unit boundary when anything completed this iteration:
        //    promote what handlers kept and reclaim their garbage.
        if completed {
            vm.promote_generation();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
