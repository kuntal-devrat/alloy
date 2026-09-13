/// Markdown documentation for Alloy keywords, runtime built-ins, and standard libraries.
pub fn get_hover_doc(symbol: &str) -> Option<(&'static str, &'static str)> {
    match symbol {
        // --- Alloy Extensions & Concurrency ---
        "channel" => Some((
            "function channel(): Channel",
            "### `channel()`\nCreates an asynchronous multi-producer multi-consumer channel for cross-actor communication.\n\n```javascript\nconst ch = channel();\nch.send(\"hello\");\nconst msg = ch.recv();\n```",
        )),
        "spawn" => Some((
            "function spawn(fn: Function, ...args: any[]): ActorHandle",
            "### `spawn(fn, ...args)`\nSpawns an isolated actor on a native operating system worker thread. Each actor has its own isolate and memory space.\n\n```javascript\nspawn((ch) => {\n    ch.send(42);\n}, ch);\n```",
        )),
        "memory" => Some((
            "module memory",
            "### `memory` (Sidecar Shared Memory)\nHigh-performance shared memory ring buffer providing sub-microsecond IPC between Alloy and Python/native processes.\n\n- `memory.create(name, size)`: Creates a named shared memory ring buffer\n- `memory.open(name)`: Opens an existing named memory segment\n- `memory.write(offset, data)`: Writes binary data\n- `memory.read(offset, len)`: Reads binary data",
        )),
        "http" => Some((
            "module http",
            "### `http` Module\nHigh-performance HTTP server backed by non-blocking asynchronous I/O and zero-copy pipelining.\n\n- `http.createServer(handler)`: Creates a new HTTP server instance\n- `server.listen(port, host?)`: Starts listening on specified port\n- `server.close()`: Gracefully stops the server",
        )),
        "fetchSync" => Some((
            "function fetchSync(url: string, options?: object): Response",
            "### `fetchSync(url, options?)`\nSynchronous HTTP/HTTPS client powered by pure-Rust TLS.\n\n```javascript\nconst res = fetchSync(\"https://api.github.com\", {\n    headers: { \"User-Agent\": \"Alloy\" }\n});\nprint(res.status, res.text());\n```\n\nReturns `{ status, statusText, ok, headers, text(), json() }`.",
        )),
        "fs" => Some((
            "module fs",
            "### `fs` Module\nFile system operations with synchronous semantics.\n\n- `fs.readFileSync(path, encoding?)`: Reads entire file content\n- `fs.writeFileSync(path, data)`: Writes data to file\n- `fs.existsSync(path)`: Checks if path exists\n- `fs.unlinkSync(path)`: Deletes file",
        )),
        "crypto" => Some((
            "module crypto",
            "### `crypto` Module\nStandard Web Cryptography API implementation.\n\n- `crypto.randomUUID()`: Generates RFC 4122 v4 UUID\n- `crypto.getRandomValues(typedArray)`: Cryptographically secure random values\n- `crypto.subtle.digest(algorithm, data)`: SHA-256/SHA-512 hashing",
        )),
        "URL" => Some((
            "class URL",
            "### `URL`\nStandard WHATWG URL constructor and parser.\n\n```javascript\nconst u = new URL(\"https://example.com:8080/path?q=1#hash\");\nprint(u.protocol, u.hostname, u.searchParams.get(\"q\"));\n```",
        )),
        "print" => Some((
            "function print(...args: any[]): void",
            "### `print(...args)`\nWrites values to standard output separated by spaces and terminated by a newline.",
        )),
        "structuredClone" => Some((
            "function structuredClone<T>(value: T): T",
            "### `structuredClone(value)`\nCreates a deep clone of the given value, correctly duplicating nested objects, arrays, and circular references.",
        )),

        // --- Standard Built-in Objects ---
        "Promise" => Some((
            "class Promise<T>",
            "### `Promise`\nRepresents the eventual completion (or failure) of an asynchronous operation.\n\n- `Promise.resolve(val)`\n- `Promise.reject(err)`\n- `Promise.all(iterable)`\n- `Promise.race(iterable)`",
        )),
        "Array" => Some((
            "class Array<T>",
            "### `Array`\nStandard JavaScript Array object.\n\n- `Array.from(iterable)`\n- `Array.isArray(obj)`\n- Methods: `push`, `pop`, `map`, `filter`, `reduce`, `slice`, `splice`, `find`, `includes`",
        )),
        "Object" => Some((
            "class Object",
            "### `Object`\nTop-level prototype and utilities.\n\n- `Object.keys(obj)`\n- `Object.values(obj)`\n- `Object.entries(obj)`\n- `Object.assign(target, ...sources)`\n- `Object.hasOwn(obj, prop)`\n- `Object.fromEntries(iterable)`",
        )),
        "Math" => Some((
            "object Math",
            "### `Math`\nMathematical constants and functions (`sin`, `cos`, `sqrt`, `floor`, `ceil`, `min`, `max`, `random`, `PI`, `E`).",
        )),
        "JSON" => Some((
            "object JSON",
            "### `JSON`\nStandard JSON serialization and parsing.\n\n- `JSON.stringify(val, replacer?, space?)`\n- `JSON.parse(text, reviver?)`",
        )),
        "Error" => Some((
            "class Error",
            "### `Error`\nRuntime error representation with stack trace support.\n\n- `new Error(message)`\n- `Error.captureStackTrace(targetObject, constructorOpt?)`\n- Properties: `name`, `message`, `stack`",
        )),

        // --- Language Keywords ---
        "async" => Some((
            "keyword async",
            "Declares an asynchronous function which returns a `Promise` and enables the `await` keyword inside its body.",
        )),
        "await" => Some((
            "keyword await",
            "Suspends execution of an `async` function until a `Promise` settles, unwrapping its fulfillment value.",
        )),
        "yield" => Some((
            "keyword yield",
            "Pauses a generator function (`function*`) and yields an expression to the caller.",
        )),
        "class" => Some((
            "keyword class",
            "Declares an ECMAScript class with support for constructors, public/private fields (`#priv`), static methods, and inheritance (`extends`).",
        )),
        "import" => Some((
            "keyword import",
            "Imports bindings exported by another module.\n\n- ES Module: `import { x } from './mod.ajs'`\n- Alloy Core: `import { http } from 'alloy:core'`\n- Python Sidecar: `import { numpy } from './math.py' as python`",
        )),
        "export" => Some((
            "keyword export",
            "Exports functions, objects, or primitive values from the module so they can be used by other programs via `import`.",
        )),
        "const" => Some((
            "keyword const",
            "Declares a block-scoped, read-only named constant.",
        )),
        "let" => Some((
            "keyword let",
            "Declares a block-scoped local variable, optionally initializing it to a value.",
        )),
        "function" => Some((
            "keyword function",
            "Defines a function declaration or function expression.",
        )),
        "return" => Some((
            "keyword return",
            "Specifies the value to be returned by a function and ends its execution.",
        )),
        _ => None,
    }
}
