# Contributing to Alloy

Thank you for your interest in contributing to Alloy! Alloy is a hyper-optimized polyglot runtime combining an ultra-fast JavaScript virtual machine, seamless Python sidecar integration, lock-free shared memory, and Cranelift JIT compilation.

## Code of Conduct

All contributors and maintainers are expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## Development Setup

### Prerequisites

- **Rust**: 1.75+ (nightly is optional, stable is fully supported).
- **Python**: 3.10+ (for Python sidecar / embed features).
- **Node.js**: v18+ (used strictly for differential testing against V8).

### Building

Clone the repository and build the workspace:

```bash
git clone https://github.com/alloy-runtime/alloy.git
cd alloy
cargo build
```

To run the binary:

```bash
# Start the interactive REPL
cargo run

# Run a script
cargo run -- examples/actor-pipeline/main.ajs

# Evaluate inline code
cargo run -- -e "console.log('Hello from Alloy!')"
```

## Running Tests

Before submitting a pull request, ensure all test suites pass:

```bash
# 1. Run all workspace unit, integration, and fuzz tests
cargo test --workspace

# 2. Verify formatting
cargo fmt --all -- --check

# 3. Verify clippy lints
cargo clippy --workspace --all-targets -- -D warnings

# 4. Run differential tests against Node.js (PowerShell / Bash)
pwsh difftest/run.ps1
```

## Repository Architecture

The codebase is organized into four core crates under `crates/`:

- **`alloy-core`**: Foundation primitives:
  - NaN-boxed `Value` representation (`value.rs`).
  - Generational arena memory management (`arena.rs`, `heap.rs`).
  - Zero-copy shared memory IPC (`shared_memory.rs`).
  - High-performance linear-time regex engine (`regex.rs`).
- **`alloy-vm`**: Bytecode compiler and execution engine:
  - Lexer, parser, AST, and bytecode codegen (`compiler/`).
  - Register-based bytecode dispatch loop (`vm/dispatch.rs`).
  - Standard built-in objects (`vm/builtins/`).
  - Module loader (`vm/modules.rs`).
  - Cranelift JIT compilation for hot loops (`jit/`).
  - Python sidecar process manager and embed bridge (`python_sidecar.rs`, `python_embed.rs`).
  - Language Server Protocol (LSP) server (`lsp/`).
- **`alloy-rt`**: Standalone asynchronous runtime services:
  - Tokio HTTP server and connection management (`server.rs`).
  - Distributed message bus (`message.rs`).
- **`alloy-cli`**: Command-line interface and developer tools:
  - CLI commands, REPL, benchmarking, disassembler (`main.rs`).
  - Package manager and dependency resolution (`pkg.rs`).

## Pull Request Guidelines

1. **Keep it Focused**: A pull request should address a single bug fix, feature, or optimization.
2. **Add Tests**: Every bug fix or new feature must be accompanied by unit tests in `crates/alloy-vm/tests/` or differential tests in `difftest/`.
3. **Format Your Code**: Run `cargo fmt --all` before pushing.
4. **Follow Commit Conventions**: Use clear, descriptive commit messages (e.g. `fix(vm): throw exception on readFileSync error`).
