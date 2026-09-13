# Alloy Standard Library & API Reference

Alloy provides an ergonomic, dependency-free standard library tailored for backend systems and web APIs.

---

## 1. `http` Module

The built-in HTTP/1.1 server engine supports high-throughput connection pooling, chunked transfer, query parameter parsing, and cookie handling.

### `http.createServer(handler)`

Creates a new HTTP server.

```javascript
import { http } from 'alloy:core';

const server = http.createServer((req, res) => {
    // Request properties:
    // req.method  -> "GET", "POST", etc.
    // req.url     -> Full request URL
    // req.path    -> URL path (e.g. "/api/users")
    // req.query   -> Parsed query string dictionary
    // req.headers -> Request headers object
    // req.cookies -> Parsed cookies dictionary
    // req.body    -> String body
    // req.ip      -> Client IP (respects X-Forwarded-For)

    if (req.method === 'POST' && req.path === '/api/login') {
        const body = JSON.parse(req.body);
        return res.status(200)
            .set('Set-Cookie', 'session=abc; HttpOnly')
            .json({ token: 'xyz123' });
    }

    res.status(404).text("Endpoint not found");
});

server.listen(8080);
```

### Response Object (`res`)

- `res.status(code)`: Sets HTTP status code (default 200).
- `res.set(header, value)`: Sets an HTTP response header.
- `res.send(data)`: Sends raw response body (string).
- `res.json(data)`: Serializes object to JSON and sets `Content-Type: application/json`.
- `res.text(str)`: Sets `Content-Type: text/plain` and sends string.
- `res.html(html)`: Sets `Content-Type: text/html` and sends HTML string.

---

## 2. `crypto` Module

Alloy ships pure-Rust cryptographic primitives with zero external npm dependencies:

```javascript
import { crypto } from 'alloy:core';

// SHA-256 Digest
const hash = crypto.sha256("hello alloy");

// HMAC-SHA256 (useful for JWT signing)
const signature = crypto.hmacSha256("payload", "secret-key");

// Timing-Safe Equality Comparison (prevents side-channel timing attacks)
const isValid = crypto.timingSafeEqual(signature, clientSignature);

// Secure Random Bytes (hex string)
const token = crypto.randomHex(32);
```

---

## 3. `fetchSync(url, options)`

Performs a synchronous or worker-isolated HTTP request:

```javascript
const res = fetchSync("http://api.example.com/data", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ query: "status" }),
    timeoutMs: 5000
});

print("Status:", res.status);
print("Body:", res.body);
```

---

## 4. `fs` Module

Synchronous and atomic filesystem operations:

```javascript
import { fs } from 'alloy:core';

// Read / Write files
fs.writeFileSync("config.json", JSON.stringify({ env: "production" }));
const content = fs.readFileSync("config.json");

// Directory and existence checks
const exists = fs.existsSync("config.json");
const files = fs.readdirSync(".");
```

---

## 5. `memory` Module

Low-level buffer management and zero-copy shared memory allocations:

```javascript
import { memory } from 'alloy:core';

// Allocate typed arrays directly inside the shared segment
const f32 = memory.allocateFloat32Array([1.0, 2.0, 3.0]);
const u8 = memory.allocateUint8Array([0x41, 0x42, 0x43]);

print("Pointer address:", f32.ptr);
print("Length:", f32.length);
```

---

## 6. Global Natives

- `print(...args)`: High-efficiency stdout writer.
- `setTimeout(callback, ms)`: Asynchronous timer queued on Tokio event loop.
- `Promise`: Full ES2022 Promise implementation (`Promise.all`, `Promise.race`, `Promise.withResolvers`).
- `Date`, `Math`, `JSON`, `RegExp`: Standard ECMAScript built-in objects.
- `btoa(str)` & `atob(b64)`: Base64 encoding/decoding.
- `encodeURIComponent(str)` & `decodeURIComponent(str)`: URL encoding helpers.
