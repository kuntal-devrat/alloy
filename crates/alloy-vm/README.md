# alloy-vm

Compiler, virtual machine, and Cranelift JIT engine for the [Alloy](https://github.com/alloy-runtime/alloy) runtime.

## Features

- **AST & Lexer/Parser**: Robust AST representations with recursion depth limits and error recovery.
- **Register-Based Bytecode VM**: Fast dispatch loop with threaded execution and optimized opcode handling.
- **Inline Caches (IC)**: Monomorphic and polymorphic property lookup acceleration.
- **Cranelift Baseline JIT**: Native machine code generation for hot execution loops using `cranelift-codegen` and `cranelift-jit`.
- **Sourcemaps & Diagnostics**: Full Sourcemap V3 mapping and rich stack trace formatting.

## Usage

```toml
[dependencies]
alloy-vm = "0.1.0"
```

## License

MIT
