# alloy-rt

Runtime event loop, actor concurrency, and asynchronous I/O engine for [Alloy](https://github.com/alloy-runtime/alloy).

## Features

- **Asynchronous Event Loop**: High-performance I/O scheduling powered by Tokio.
- **Actor Concurrency Model**: Isolated actors communicating via asynchronous MPSC channels (`spawn()`, `channel()`).
- **HTTP/1.1 Engine**: Built-in HTTP server supporting keep-alive, pipelining, chunked transfer, and TLS-terminating proxies.
- **Polyglot Bridge**: Python sidecar child process management and zero-copy pointer passing.

## Usage

```toml
[dependencies]
alloy-rt = "0.2.0"
```

## License

MIT
