# alloy-core

Core foundation library for the [Alloy](https://github.com/alloy-runtime/alloy) runtime.

## Features

- **NaN-Tagged Values**: Compact 64-bit IEEE 754 value representation encoding booleans, integers, null, undefined, pointers, and symbols without dynamic memory allocation overhead.
- **Arena Allocator**: High-performance bump-pointer arena for ephemeral per-request memory allocation, enabling instant $O(1)$ reclamation without tracing GC pauses.
- **Shared Memory Segment**: Cross-process, file-backed shared memory implementation (`CreateFileMappingW` on Windows, `mmap` / POSIX shm on Unix) enabling zero-copy data exchange with Python, C, and Rust sidecars.
- **String Interning**: Lock-free string deduplication and slice storage (`AString`).

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
alloy-core = "0.2.0"
```

## License

MIT
