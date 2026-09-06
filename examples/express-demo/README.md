# express-demo — the Express API on alloy

`lib/express.ajs` is an Express-compatible framework (zero dependencies) for
alloy; this demo tours it: middleware order, `:params`, `Router` mount, error
middleware, JSON validation, cookies, static files.

```sh
cargo build
ALLOY_VM_BUDGET=0 ./target/debug/alloy examples/express-demo/server.ajs  # :8091
ALLOY_VM_BUDGET=0 ./target/debug/alloy examples/express-demo/smoke.ajs   # 12 checks
```

## Express compatibility

| Express | alloy | notes |
|---|---|---|
| `app.get/post/put/del/all`, `app.use`, `app.listen` | ✅ | `del` (not `delete`: reserved word) |
| `next()` / `next(err)`, 4-arg error middleware | ✅ | arity via `fn.length` (added to runtime) |
| `res.status().json()/send()/text()/html()` chains | ✅ | + `res.cookie/redirect/type` |
| `req.params/query/cookies/headers/ip`, `req.get()` | ✅ | `query`/`cookies` pre-parsed by runtime |
| `Router()` + `app.use('/prefix', router)` | ✅ | prefix strip/restore, errors propagate up |
| `express.json()/cors()/static()` | ✅ | as **named imports** (no fn statics in engine) |
| `next('route')`, sub-apps, views, `urlencoded()` | ❌ | documented in `lib/express.ajs` header |

`X-Powered-By: alloy` is set unless `app.set("x-powered-by", false)`.
Async route handlers: rejections route to error middleware; middleware itself
must call `next()` synchronously (Express 4 rule).
