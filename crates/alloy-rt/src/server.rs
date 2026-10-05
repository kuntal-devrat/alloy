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
    pub workers: usize,
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
            workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
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

    pub fn should_keep_alive(&self, default_keep_alive: bool) -> bool {
        if let Some(conn) = self.header("connection") {
            if conn.eq_ignore_ascii_case("close") {
                return false;
            }
            if conn.eq_ignore_ascii_case("keep-alive") {
                return true;
            }
        }
        default_keep_alive
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
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    // Chunked Transfer-Encoding
    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    if is_chunked {
        let (body, consumed) = alloy_vm::vm::http::decode_chunked_body(&buf[header_len..])?;
        let total_needed = header_len + consumed;
        let raw = buf[..total_needed].to_vec();
        return Some((
            HttpRequest {
                method,
                path,
                headers,
                body,
                raw,
            },
            total_needed,
        ));
    }

    // Content-Length
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
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
    Some((
        HttpRequest {
            method,
            path,
            headers,
            body,
            raw,
        },
        total_needed,
    ))
}

fn build_response(
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", status, reason).as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    if keep_alive {
        out.extend_from_slice(b"Connection: keep-alive\r\n");
    } else {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    for (k, v) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn bind_reuseport_listener(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv6() {
        tokio::net::TcpSocket::new_v6()?
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    {
        socket.set_reuseport(true)?;
    }
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
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

    pub fn with_workers(mut self, workers: usize) -> Self {
        self.config.workers = workers.max(1);
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

    async fn run_accept_loop(
        listener: TcpListener,
        typed: Option<Arc<dyn Fn(HttpRequest) -> Vec<u8> + Send + Sync>>,
        raw: Option<Arc<dyn Fn(Vec<u8>) -> Vec<u8> + Send + Sync>>,
        cfg: ServerConfig,
    ) {
        loop {
            let (mut stream, addr) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    if std::env::var_os("ALLOY_TRACE").is_some() {
                        eprintln!("[alloy] accept error: {}", e);
                    }
                    continue;
                }
            };
            if std::env::var_os("ALLOY_TRACE").is_some() {
                eprintln!("[alloy] connection from {}", addr);
            }
            let typed = typed.clone();
            let raw = raw.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(8192);
                let mut tmp = vec![0u8; 8192];
                'conn: loop {
                    let mut consumed = 0usize;
                    let mut parsed_any = false;
                    let mut should_keep_conn = cfg.keep_alive;
                    while let Some((req, needed)) = parse_request(&buf[consumed..]) {
                        parsed_any = true;
                        let keep_alive = cfg.keep_alive && req.should_keep_alive(cfg.keep_alive);
                        should_keep_conn = keep_alive;
                        let response = if let Some(h) = &typed {
                            h(req)
                        } else if let Some(h) = &raw {
                            let raw_bytes = buf[consumed..consumed + needed].to_vec();
                            h(raw_bytes)
                        } else {
                            build_response(
                                200,
                                "OK",
                                &[("Content-Type", "text/plain")],
                                b"Hello, alloy!",
                                keep_alive,
                            )
                        };
                        let framed = if response.starts_with(b"HTTP/") {
                            response
                        } else {
                            build_response(
                                200,
                                "OK",
                                &[("Content-Type", "text/plain")],
                                &response,
                                keep_alive,
                            )
                        };
                        if timeout(cfg.write_timeout, stream.write_all(&framed))
                            .await
                            .is_err()
                        {
                            break 'conn;
                        }
                        if timeout(cfg.write_timeout, stream.flush()).await.is_err() {
                            break 'conn;
                        }
                        consumed += needed;
                        if !keep_alive {
                            let _ = stream.shutdown().await;
                            return;
                        }
                    }

                    if consumed > 0 {
                        buf.drain(..consumed);
                    }

                    if parsed_any && !should_keep_conn {
                        break 'conn;
                    }

                    if buf.len() > cfg.max_header_bytes + cfg.max_body_bytes {
                        let resp = build_response(
                            413,
                            "Payload Too Large",
                            &[],
                            b"Payload Too Large",
                            false,
                        );
                        let _ = timeout(cfg.write_timeout, stream.write_all(&resp)).await;
                        break 'conn;
                    }
                    if buf.len() > cfg.max_header_bytes && !buf.windows(4).any(|w| w == b"\r\n\r\n")
                    {
                        let resp = build_response(
                            431,
                            "Request Header Fields Too Large",
                            &[],
                            b"Header Too Large",
                            false,
                        );
                        let _ = timeout(cfg.write_timeout, stream.write_all(&resp)).await;
                        break 'conn;
                    }

                    let wait_timeout = if parsed_any && buf.is_empty() {
                        cfg.keep_alive_timeout
                    } else {
                        cfg.read_timeout
                    };

                    let read_fut = stream.read(&mut tmp);
                    let n = match timeout(wait_timeout, read_fut).await {
                        Ok(Ok(0)) => break 'conn,
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) => break 'conn,
                        Err(_) => break 'conn,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                }
            });
        }
    }

    pub async fn listen(&self) -> Result<(), Box<dyn std::error::Error>> {
        use std::net::ToSocketAddrs;
        let socket_addr = self.addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid address")
        })?;

        let workers = self.config.workers;
        if workers > 1 {
            println!(
                "[alloy] listening on {} (workers={}, SO_REUSEPORT=enabled, keep_alive={})",
                self.addr, workers, self.config.keep_alive
            );

            let mut handles = Vec::with_capacity(workers);
            for worker_id in 0..workers {
                let typed = self.handler.clone();
                let raw = self.raw_handler.clone();
                let cfg = self.config.clone();
                let addr = socket_addr;

                let handle = std::thread::Builder::new()
                    .name(format!("alloy-worker-{}", worker_id))
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("failed to create worker runtime");

                        rt.block_on(async move {
                            let listener = match bind_reuseport_listener(addr) {
                                Ok(l) => l,
                                Err(e) => {
                                    eprintln!(
                                        "[alloy] worker {} failed to bind SO_REUSEPORT: {}",
                                        worker_id, e
                                    );
                                    return;
                                }
                            };
                            Self::run_accept_loop(listener, typed, raw, cfg).await;
                        });
                    })?;
                handles.push(handle);
            }

            for h in handles {
                let _ = h.join();
            }
            Ok(())
        } else {
            let listener = match bind_reuseport_listener(socket_addr) {
                Ok(l) => l,
                Err(_) => TcpListener::bind(socket_addr).await?,
            };
            println!(
                "[alloy] listening on {} (single-worker, keep_alive={})",
                self.addr, self.config.keep_alive
            );
            Self::run_accept_loop(
                listener,
                self.handler.clone(),
                self.raw_handler.clone(),
                self.config.clone(),
            )
            .await;
            Ok(())
        }
    }
}

// backwards compat helper
impl Default for HttpServer {
    fn default() -> Self {
        Self::new("127.0.0.1:8080")
    }
}
