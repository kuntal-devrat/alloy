# Alloy VS Code Extension

Official Visual Studio Code extension for **Alloy** — the hyper-optimized polyglot systems runtime.

## Features

- **Syntax Highlighting**: Full grammar support for `.ajs` and `.alloy` files, including JS syntax, classes, private properties (`#field`), async/await, generators, and Alloy extensions (`channel`, `spawn`, Python sidecar imports).
- **Diagnostics**: Real-time syntax errors and parse warnings reported via the built-in Language Server (`alloy lsp`).
- **Hover Documentation**: Detailed documentation, signatures, and code examples for Alloy built-in modules (`http`, `memory`, `fs`, `crypto`, `URL`, `fetchSync`, `print`, `spawn`, `channel`) and keywords.
- **Auto-Completions**: Intelligent completions for keywords, global namespaces, and standard objects.
- **Document Symbols & Outline**: Interactive outline view for classes, methods, functions, and declared variables.

## Requirements

Ensure `alloy` is installed and available in your system `PATH`, or set `alloy.executablePath` in your VS Code settings.
