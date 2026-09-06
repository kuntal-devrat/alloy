// Node baseline for the bridge microbench: 20 spawn+JSON round-trips.
const { execFileSync } = require("child_process");
const path = require("path");
const PY = process.env.ALLOY_PYTHON || "python";
const pixels = [];
for (let i = 0; i < 1024; i++) { pixels.push(((i % 200) - 100) / 100); }
const payload = JSON.stringify({ pixels });
const t0 = Date.now();
for (let i = 0; i < 20; i++) {
  execFileSync(PY, [path.join(__dirname, "model_cli.py")], { input: payload, encoding: "utf8" });
}
console.log(Date.now() - t0);
