// t03: loops — for/while/for-of/for-in, labeled break and continue, nested.
let sum = 0;
for (let i = 1; i <= 100; i++) { sum += i; }
print("sum", sum);
let w = 0;
while (w < 10) { w += 2; }
print("while", w);
let m = 0;
outer: for (let i = 0; i < 3; i++) {
    for (let j = 0; j < 3; j++) {
        if (i * j > 2) { break outer; }
        m += 1;
    }
}
print("labeled-break", m);
let n = 0;
lbl: for (let i = 0; i < 5; i++) {
    if (i === 2) { continue lbl; }
    n += 1;
}
print("labeled-continue", n);
let evens = "";
for (let i = 0; i < 10; i++) {
    if (i % 2 === 1) { continue; }
    evens = evens + i;
}
print("evens", evens);
let total = 0;
for (let v of [1, 2, 3, 4]) { total += v; }
print("forof", total);
let str = "";
for (let ch of "ab") { str = str + ch; }
print("forof-str", str);
let keylist = "";
for (let k in { a: 1, b: 2, c: 3 }) { keylist = keylist + k; }
print("forin", keylist);
let idx = 0;
while (true) {
    idx += 1;
    if (idx === 7) { break; }
}
print("while-true", idx);
let skips = "";
for (let i = 0; i < 6; i++) {
    if (i === 1 || i === 4) { continue; }
    skips = skips + i;
}
print("skips", skips);
let outerCount = 0;
for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 4; j++) {
        if (j > i) { break; }
        outerCount += 1;
    }
}
print("triangular", outerCount);
