# alloy-cli

Command-line interface, REPL, package manager, and Language Server Protocol daemon for the [Alloy](https://github.com/alloy-runtime/alloy) runtime.

## Installation

```bash
cargo install alloy-cli
```

## Commands

```bash
alloy run <file.ajs>    # Execute an Alloy or JavaScript script
alloy repl              # Start an interactive REPL
alloy bench <file.ajs>  # Run benchmark with microsecond timing
alloy test              # Discover and run *.test.ajs test suites
alloy pkg               # Manage dependencies (alloy.json)
alloy add <pkg>         # Add a new package dependency
alloy init <name>       # Scaffold a new Alloy project
alloy lsp               # Launch JSON-RPC Language Server Protocol daemon
```

## License

MIT
