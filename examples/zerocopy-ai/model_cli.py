# Same algorithm as model.py, but JSON-in/JSON-out over stdio.
# Used by the Node baseline (spawn-per-request): the classic FFI tax —
# serialize the whole vector to JSON, spawn python, parse it back.
import sys, json

LABELS = ("dark", "mid", "bright")

def _softmax(xs):
    m = max(xs)
    ex = [2.718281828459045 ** (x - m) for x in xs]
    s = sum(ex)
    return [e / s for e in ex]

def main():
    payload = json.load(sys.stdin)
    vals = payload["pixels"]
    n = len(vals)
    s = sum(vals)
    a = sum(v if v >= 0 else -v for v in vals)
    mean = s / n
    energy = a / n
    logits = (mean * 4.0, 1.0 - abs(mean) * 2.0 + energy, -mean * 4.0)
    m = max(logits)
    ex = [2.718281828459045 ** (x - m) for x in logits]
    tot = sum(ex)
    probs = [e / tot for e in ex]
    best = max(range(3), key=lambda i: probs[i])
    json.dump({"label": "{} {:.4f}".format(LABELS[best], probs[best])}, sys.stdout)

if __name__ == "__main__":
    main()
