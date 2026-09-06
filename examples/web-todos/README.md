# web-todos — a complete web app on alloy. No deps.

A JSON API + static frontend + JWT auth + persistence, running on the alloy
runtime with zero dependencies. Proves the answer to "can we use it for
web applications?" is **yes for API-shaped apps**.

```
examples/web-todos/
  server.ajs        # routes, auth gate, JSON-file DB (fs), static fallback
  lib/router.ajs    # method + :param router, middleware chain
  lib/auth.ajs      # HS256 JWT sign/verify (crypto), Bearer + cookie
  lib/static.ajs    # traversal-guarded static files + content types
  lib/ratelimit.ajs # sliding-window limiter + CORS middleware
  public/index.html # browser UI (uses browser fetch, no build step)
  Caddyfile         # TLS termination (alloy speaks plain HTTP)
```

## Run

```sh
cargo build
ALLOY_VM_BUDGET=0 ./target/debug/alloy examples/web-todos/server.ajs  # :8090
curl localhost:8090/health
curl -X POST localhost:8090/api/login -d '{"user":"ada"}'
# -> {"token":"eyJ..."}  (use it as Authorization: Bearer <t>)
curl -X POST localhost:8090/api/todos -H "Authorization: Bearer <t>" \
  -d '{"title":"buy milk"}'   # -> 201
open http://localhost:8090/   # click todos to toggle
```

## What the platform now provides (shipped with this example)

| need | alloy surface |
|---|---|
| routing | `req.{method, path, query}`, `:param` router lib |
| responses | `res.{send, json, text, html, status(code), set(k,v)}` |
| headers/cookies | `req.headers` (lowercased), `req.cookies`, `req.ip`, `req.protocol` |
| outgoing HTTP | `fetchSync(url, {method, headers, body, timeoutMs})` + `spawn()` for concurrency |
| hashing/auth | `crypto.{sha256, hmacSha256, hmacBase64Url, randomHex, base64*, timingSafeEqual}` |
| URLs | `URL.parse(href, base?)`, `encode/decodeURIComponent`, `encode/decodeURI`, `btoa/atob` |
| static | `alloy:fs` + content-type lib (traversal-guarded) |
| TLS | terminate at Caddy/nginx; `X-Forwarded-Proto/For` trusted into `req` |

JWTs verify against any HS256 implementation (checked with Python hashlib).

## Known limits (honest)

- Plain HTTP only — put Caddy/nginx in front for HTTPS (Caddyfile included).
- `Connection: close` per request (no keep-alive yet); `fs` is sync-only.
- No websockets/SSE, no multipart uploads, no npm ecosystem (no Express/Passport/ORM).
- Hot compute is 5–40× slower than V8 JIT (`bench/RESULTS.md`) — fine for
  I/O-bound APIs, not for render-heavy SSR loops.
- Engine quirk found while building this: object-literal methods written with
  `function` don't capture the enclosing `const` self-name
  (`const o = { m: function(){ return o; } }` → ReferenceError). The router
  lib works around it via method-call `this`. Arrow closures capture fine.
