// t01: arithmetic, precedence, ternary, short-circuit, ++/--, compound and
// chained assignment. Deterministic scalar output; no display-sensitive values.
let a = 5;
let b = a++ + 2;
let c = ++a + 2;
a += 4;
b -= 3;
let d = 2;
let e = d = 10;
let f = 1 + 2 * 3 - 4 / 2 + 5 % 3;
let g = true ? 10 : 20;
let h = false ? 1 : true ? 2 : 3;
let i = 0 || "fallback";
let j = "x" && "y";
let k = 1e3 + 2E2 - 1.5e-3;
let l = 7 / 2;
let m = 7 % 2;
let n = 0.1 + 0.2;
let o = 5 % 0;
let p = -5 % 3;
print(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p);
let q = 3 - 5;
let r = -(3 - 5);
let s = 2 * 3 + 4 * 5;
let t = (2 + 3) * (4 + 5);
print(q, r, s, t);
print(1 + "2", "3" + 4, 5 + 6 + "7", "8" + 9 + 10);
