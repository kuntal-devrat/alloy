# Getting Started with Alloy

Alloy is a polyglot systems runtime written in Rust designed for speed, memory efficiency, and fearless concurrency.

---

## 1. Installation

### One-Line Installers

#### Linux & macOS
```bash
curl -fsSL https://raw.githubusercontent.com/alloy-runtime/alloy/main/install.sh | sh
```

#### Windows (PowerShell)
```powershell
irm https://raw.githubusercontent.com/alloy-runtime/alloy/main/install.ps1 | iex
```

### Build from Source

Ensure you have Rust 1.75+ installed:

```bash
git clone https://github.com/alloy-runtime/alloy.git
cd alloy
cargo build --release -p alloy-cli
cp target/release/alloy ~/.alloy/bin/
```

Verify your installation:

```bash
alloy --version
# Output: alloy 0.2.0
```

---

## 2. File Extensions

Alloy natively uses the `.ajs` file extension (*Alloy JavaScript*):
- **`.ajs`**: Modern JavaScript source with Alloy system natives (`alloy:core`, `spawn`, `channel`, `memory`, `http`).
- **`.js`**: Standard JavaScript files (fully supported for backward compatibility).
- **`.ax`**: Precompiled, optimized binary bytecode format generated via `alloy --emit-ax`.

---

## 3. CLI Commands

### Execute Code

Run a script directly:
```bash
alloy run app.ajs
# or simply
alloy app.ajs
```

### Interactive REPL

Launch the interactive REPL with live evaluation, persistent globals, and multiline expression support:
```bash
alloy repl
```

```
alloy v0.2.0 REPL (type 'exit' to quit)
alloy> const x = 42;
alloy> x * 2
84
alloy> const [tx, rx] = channel();
alloy> tx.send("hello from repl");
alloy> rx.recv()
'hello from repl'
```

### Benchmarking

Measure execution time with microsecond resolution and cold-start breakdown:
```bash
alloy bench app.ajs
```

### Test Runner

Run unit and integration test files matching `*.test.ajs` or `*.test.js`:
```bash
alloy test
alloy test auth  # Filter tests matching "auth"
```

### Precompile Bytecode

Compile an `.ajs` script into portable `.ax` bytecode for near-zero boot time:
```bash
alloy --emit-ax app.ajs app.ax
alloy run app.ax
```

### Package Management

Initialize a project:
```bash
alloy init my-api
cd my-api
```

Add an npm dependency via Alloy's pure-JS compatibility layer:
```bash
alloy add lodash
```

Install dependencies declared in `alloy.json`:
```bash
alloy install
```

### Language Server (LSP)

Start the JSON-RPC Language Server Protocol over stdin/stdout for VS Code, Neovim, or Helix:
```bash
alloy lsp
```

---

## 4. Your First Alloy Application

Create `main.ajs`:

```javascript
import { http, memory, channel } from 'alloy:core';

print("Initializing Alloy server...");

http.createServer((req, res) => {
    if (req.path === '/api/ping') {
        return res.json({ status: 'ok', time: Date.now() });
    }

    res.status(404).text("Not Found");
}).listen(3000);

print("Listening on http://127.0.0.1:3000");
```

Run it:
```bash
alloy run main.ajs
```
