use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub keep_alive: bool,
    pub keep_alive_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            max_header_bytes: 16 * 1024,
            max_body_bytes: 10 * 1024 * 1024,
            keep_alive: true,
            keep_alive_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub raw: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        for (k, v) in &self.headers {
            if k.to_ascii_lowercase() == n {
                return Some(v.as_str());
            }
        }
        None
    }
}

fn parse_request(buf: &[u8]) -> Option<(HttpRequest, usize)> {
    // Find header end \r\n\r\n
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header_len = header_end + 4;
    if header_len > 16 * 1024 * 4 {
        return None;
    }
    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let _version = parts.next()?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    // Content-Length
    let content_length: usize = headers.iter()
        .find(|(k,_)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_,v)| v.parse().ok())
        .unwrap_or(0);
    if content_length > 10 * 1024 * 1024 {
        return None;
    }
    let total_needed = header_len + content_length;
    if buf.len() < total_needed {
        return None; // need more data
    }
    let body = buf[header_len..total_needed].to_vec();
    let raw = buf[..total_needed].to_vec();
    Some((HttpRequest { method, path, headers, body, raw }, total_needed))
}

fn build_response(status: u16, reason: &str, headers: &[(&str,&str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: keep-alive\r\n");
    for (k,v) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

pub struct HttpServer {
    addr: String,
    config: ServerConfig,
    handler: Option<Arc<dyn Fn(HttpRequest) -> Vec<u8> + Send + Sync>>,
    raw_handler: Option<Arc<dyn Fn(Vec<u8>) -> Vec<u8> + Send + Sync>>,
}

impl HttpServer {
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
            config: ServerConfig::default(),
            handler: None,
            raw_handler: None,
        }
    }

    pub fn with_config(mut self, cfg: ServerConfig) -> Self {
        self.config = cfg;
        self
    }

    pub fn set_handler<F>(&mut self, handler: F)
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        self.raw_handler = Some(Arc::new(handler));
    }

    pub fn set_typed_handler<F>(&mut self, handler: F)
    where
        F: Fn(HttpRequest) -> Vec<u8> + Send + Sync + 'static,
    {
        self.handler = Some(Arc::new(handler));
    }

    pub async fn listen(&self) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(&self.addr).await?;
        println!("[alloy] listening on {} (keep_alive={})", self.addr, self.config.keep_alive);
        let typed = self.handler.clone();
        let raw = self.raw_handler.clone();
        let cfg = self.config.clone();

        loop {
            let (mut stream, addr) = listener.accept().await?;
            eprintln!("[alloy] connection from {}", addr);
            let typed = typed.clone();
            let raw = raw.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(8192);
                let mut tmp = vec![0u8; 8192];
                loop {
                    // read with timeout
                    let read_fut = stream.read(&mut tmp);
                    let n = match timeout(cfg.read_timeout, read_fut).await {
                        Ok(Ok(0)) => break, // closed
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) => break,
                        Err(_) => break, // timeout
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > cfg.max_header_bytes + cfg.max_body_bytes {
                        let resp = build_response(413, "Payload Too Large", &[], b"Payload Too Large");
                        let _ = timeout(cfg.write_timeout, stream.write_all(&resp)).await;
                        break;
                    }
                    // Try parse one request (support pipelining: loop)
                    let mut consumed = 0usize;
                    let mut parsed_any = false;
                    while let Some((req, needed)) = parse_request(&buf[consumed..]) {
                        parsed_any = true;
                        let response = if let Some(h) = &typed {
                            h(req)
                        } else if let Some(h) = &raw {
                            // raw handler compat: receives raw bytes
                            let raw_bytes = buf[consumed..consumed+needed].to_vec();
                            h(raw_bytes)
                        } else {
                            build_response(200, "OK", &[("Content-Type","text/plain")], b"Hello, alloy!")
                        };
                        // Ensure response is framed; if handler returned only body, frame it
                        let framed = if response.starts_with(b"HTTP/") { response } else { build_response(200, "OK", &[("Content-Type","text/plain")], &response) };
                        if timeout(cfg.write_timeout, stream.write_all(&framed)).await.is_err() {
                            break;
                        }
                        if timeout(cfg.write_timeout, stream.flush()).await.is_err() {
                            break;
                        }
                        consumed += needed;
                        if !cfg.keep_alive {
                            let _ = stream.shutdown().await;
                            return;
                        }
                    }
                    if parsed_any {
                        // remove consumed bytes
                        buf.drain(..consumed);
                        // if keep-alive, continue reading next request; else break
                        // idle timeout for keep-alive
                        if buf.is_empty() && cfg.keep_alive {
                            // wait briefly for next request
                            match timeout(cfg.keep_alive_timeout, stream.read(&mut tmp)).await {
                                Ok(Ok(0)) => break,
                                Ok(Ok(m)) => { buf.extend_from_slice(&tmp[..m]); continue; }
                                Ok(Err(_)) => break,
                                Err(_) => break, // keep-alive timeout -> close
                            }
                        }
                    } else {
                        // incomplete request: continue reading unless header too large
                        if buf.len() > cfg.max_header_bytes && !buf.windows(4).any(|w| w==b"\r\n\r\n") {
                            let resp = build_response(431, "Request Header Fields Too Large", &[], b"Header Too Large");
                            let _ = timeout(cfg.write_timeout, stream.write_all(&resp)).await;
                            break;
                        }
                        // need more data, loop to read again (with outer read we already did one; continue)
                        // For now, if not parsed and buf has no complete header, just continue outer loop which will read again.
                        // To avoid tight loop, we already read one chunk; if still incomplete, next iteration will read more.
                        // So break inner parse and let outer read more if not enough.
                        if buf.windows(4).any(|w| w==b"\r\n\r\n") {
                            // has header but body incomplete — need more reads; continue outer
                        }
                    }
                    // If we consumed everything and keep-alive, outer loop will read next request.
                    // If keep-alive disabled, close.
                    if !cfg.keep_alive && parsed_any {
                        break;
                    }
                    // prevent tight loop when no progress and no data: wait for more
                    if !parsed_any && buf.len() < cfg.max_header_bytes {
                        // need more data, continue to next read iteration (we already have buf)
                        // timeout will handle stall
                    }
                    // For non-keep-alive after one response, close
                    if parsed_any && !cfg.keep_alive { break; }
                    // If we reach here with parsed response sent and keep-alive on, loop will handle next recv via top of loop
                    // To avoid double-read, we already did extra read for keep-alive idle; if not idle, loop continues to outer read
                    break; // outer will re-enter read; for simplicity handle one batch per accept iteration
                }
            });
        }
    }
}

// backwards compat helper
impl Default for HttpServer {
    fn default() -> Self { Self::new("127.0.0.1:8080") }
}
