"""Bench zerocopy-ai: alloy (persistent zero-copy sidecar) vs node (spawn+JSON).

Usage:  python bench.py [--n 1024] [--reqs 100]

Starts both servers as subprocesses, waits for readiness, POSTs the same
payload N times (sequential — measures per-request latency, not throughput),
verifies identical labels, prints p50/p95/mean + speedup. Kills servers after.
Stdlib only.
"""
import json, subprocess, sys, time, urllib.request, pathlib

HERE = pathlib.Path(__file__).parent
ALLOY_BIN = HERE / ".." / ".." / "target" / "release" / ("alloy.exe" if sys.platform == "win32" else "alloy")
ALLOY_BIN = ALLOY_BIN.resolve()

def wait_ready(url, timeout=20.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.2)
    return False

def post(url, payload):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=60) as r:
        body = json.loads(r.read().decode())
    return (time.perf_counter() - t0) * 1000, body

def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]

def main():
    n = 1024
    reqs = 100
    for i, a in enumerate(sys.argv[1:]):
        if a == "--n" and i + 2 < len(sys.argv):
            n = int(sys.argv[i + 2])
        if a == "--reqs" and i + 2 < len(sys.argv):
            reqs = int(sys.argv[i + 2])
    pixels = [((i % 200) - 100) / 100.0 for i in range(n)]
    payload = {"pixels": pixels}
    print("payload: {} floats (~{:.0f} KB json)".format(n, len(json.dumps(payload)) / 1024))

    env = dict(__import__("os").environ)
    env["ALLOY_VM_BUDGET"] = "0"
    env["ALLOY_SHM_CAP"] = str(16 * 1024 * 1024)
    alloy = subprocess.Popen([str(ALLOY_BIN), str(HERE / "server.ajs")],
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env)
    node = subprocess.Popen(["node", str(HERE / "server.node.js")],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        if not wait_ready("http://127.0.0.1:8081/"):
            print("alloy server did not start"); print_tail(alloy); sys.exit(1)
        if not wait_ready("http://127.0.0.1:8082/"):
            print("node server did not start"); print_tail(node); sys.exit(1)
        # warm up (spawn pool + JIT)
        post("http://127.0.0.1:8081/classify", payload)
        post("http://127.0.0.1:8082/classify", payload)
        a_ts, n_ts = [], []
        a_label = n_label = None
        for _ in range(reqs):
            dt, body = post("http://127.0.0.1:8081/classify", payload)
            a_ts.append(dt); a_label = body.get("label")
            dt, body = post("http://127.0.0.1:8082/classify", payload)
            n_ts.append(dt); n_label = body.get("label")
        print("labels agree:", a_label == n_label, "| alloy:", a_label, "| node:", n_label)
        def row(name, xs):
            return "{:<6} mean {:7.1f}ms  p50 {:7.1f}ms  p95 {:7.1f}ms".format(
                name, sum(xs) / len(xs), pct(xs, 0.5), pct(xs, 0.95))
        print(row("alloy", a_ts))
        print(row("node", n_ts))
        print("speedup: {:.1f}x mean, {:.1f}x p50, {:.1f}x p95".format(
            (sum(n_ts) / len(n_ts)) / (sum(a_ts) / len(a_ts)),
            pct(n_ts, 0.5) / max(pct(a_ts, 0.5), 1e-9),
            pct(n_ts, 0.95) / max(pct(a_ts, 0.95), 1e-9)))
    finally:
        for p in (alloy, node):
            try: p.terminate()
            except Exception: pass
        for p in (alloy, node):
            try: p.wait(timeout=5)
            except Exception:
                try: p.kill()
                except Exception: pass

def print_tail(p):
    try:
        import select
        print("(server output unavailable on this platform)")
    except Exception:
        pass

if __name__ == "__main__":
    main()
