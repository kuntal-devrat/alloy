// bench: object property churn — gets/sets, nested objects, computed keys
let o = { a: 1, b: 2, c: 3 };
let acc = 0;
for (let i = 0; i < 600000; i++) {
    let k = i % 3;
    if (k === 0) { o.a += 1; }
    else if (k === 1) { o.b += 1; }
    else { o.c += 1; }
    acc += o.a + o.b + o.c;
}
let nested = { root: { level: { count: 0 } } };
for (let i = 0; i < 200000; i++) {
    nested.root.level.count += 1;
}
let keyed = {};
for (let i = 0; i < 100000; i++) {
    let key = "k" + (i % 100);
    if (keyed[key] === undefined) { keyed[key] = 0; }
    keyed[key] += 1;
}
let keyTotal = 0;
for (let i = 0; i < 100; i++) { keyTotal += keyed["k" + i]; }
print(acc % 100000, nested.root.level.count, keyTotal);
