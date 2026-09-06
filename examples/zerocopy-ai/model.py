# Toy 3-class softmax classifier (stdlib only — no numpy needed).
#
# Stands in for ANY real model: the point of this demo is the *bridge cost*,
# not accuracy. `classify(ptr, n)` reads the input vector straight out of the
# shared segment (zero copy — `read_bytes` is injected into this module's
# namespace by alloy's sidecar bootstrap) and returns "label prob".
#
# Same math lives in model_cli.py (JSON-in/JSON-out) for the Node baseline,
# so both engines run an identical algorithm and only the bridge differs.

LABELS = ("dark", "mid", "bright")

def _softmax(xs):
    m = max(xs)
    ex = [2.718281828459045 ** (x - m) for x in xs]
    s = sum(ex)
    return [e / s for e in ex]

def classify(ptr, n):
    import struct
    raw = read_bytes(ptr, n * 4)  # noqa: F821 — injected by alloy bootstrap
    vals = struct.unpack('<' + 'f' * n, raw)
    # Three cheap prototype scores over the raw pixels.
    s = 0.0
    a = 0.0
    for v in vals:
        s += v
        a += v if v >= 0 else -v
    mean = s / n
    energy = a / n
    logits = (mean * 4.0, 1.0 - abs(mean) * 2.0 + energy, -mean * 4.0)
    probs = _softmax(logits)
    best = max(range(3), key=lambda i: probs[i])
    return "{} {:.4f}".format(LABELS[best], probs[best])
