// bench: closure and higher-order overhead — counters, map/filter/reduce
function makeCounter() {
    let count = 0;
    return () => { count += 1; return count; };
}
let c = makeCounter();
let total = 0;
for (let i = 0; i < 1000000; i++) { total += c(); }
function map(arr, fn) {
    let out = [];
    for (let i = 0; i < arr.length; i++) { out[i] = fn(arr[i]); }
    return out;
}
function filter(arr, pred) {
    let out = [];
    for (let i = 0; i < arr.length; i++) {
        if (pred(arr[i])) { out[out.length] = arr[i]; }
    }
    return out;
}
function reduce(arr, init, fn) {
    let acc = init;
    for (let i = 0; i < arr.length; i++) { acc = fn(acc, arr[i]); }
    return acc;
}
let nums = [];
for (let i = 0; i < 20000; i++) { nums[i] = i; }
let m2 = 0;
for (let iter = 0; iter < 20; iter++) {
    let sq = map(nums, (x) => x * x);
    let even = filter(nums, (x) => x % 2 === 0);
    let r = reduce(sq, 0, (a, b) => a + b);
    m2 += r % 1000 + even.length;
}
let curried = 0;
function add(a) { return (b) => a + b; }
for (let i = 0; i < 200000; i++) { curried += add(i % 100)(1); }
print(total, m2, curried);
