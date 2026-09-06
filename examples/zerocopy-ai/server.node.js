// Node baseline for zerocopy-ai: identical API + identical math, classic bridge.
// Every request: JSON-serialize pixels -> spawn `python model_cli.py` ->
// parse JSON result. This is what you pay without a zero-copy sidecar.
const http = require("http");
const { execFileSync } = require("child_process");
const path = require("path");

const MODEL = path.join(__dirname, "model_cli.py");
const PY = process.env.ALLOY_PYTHON || "python";

const server = http.createServer((req, res) => {
  if (req.method === "GET") {
    res.writeHead(200, { "Content-Type": "application/json", Connection: "close" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }
  if (req.method === "POST") {
    let raw = "";
    req.on("data", (c) => { raw += c; });
    req.on("end", () => {
      try {
        const out = execFileSync(PY, [MODEL], { input: raw, encoding: "utf8" });
        res.writeHead(200, { "Content-Type": "application/json", Connection: "close" });
        res.end(out);
      } catch (e) {
        res.writeHead(500, { "Content-Type": "application/json", Connection: "close" });
        res.end(JSON.stringify({ error: "python failed" }));
      }
    });
    return;
  }
  res.writeHead(200, { "Content-Type": "application/json", Connection: "close" });
  res.end(JSON.stringify({ ok: true }));
});

server.listen(8082, "127.0.0.1", () => console.log("node listening on 127.0.0.1:8082"));
