// t04: functions — recursion, closures/upvalues, higher-order, IIFE, memo.
function fib(n) {
    if (n < 2) { return n; }
    return fib(n - 1) + fib(n - 2);
}
print("fib", fib(20));
function makeCounter() {
    let count = 0;
    return () => { count += 1; return count; };
}
let c1 = makeCounter();
let c2 = makeCounter();
c1();
c1();
c2();
print("counters", c1(), c2());
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
let nums = [1, 2, 3, 4, 5];
let sq = map(nums, (x) => x * x);
print("map", sq[0], sq[1], sq[4]);
let even = filter(nums, (x) => x % 2 === 0);
print("filter", even.length, even[0], even[1]);
print("reduce", reduce(nums, 0, (a, b) => a + b));
print("reduce-mul", reduce(nums, 1, (a, b) => a * b));
function compose(f, g) {
    return (x) => f(g(x));
}
let add1 = (x) => x + 1;
let dbl = (x) => x * 2;
print("compose", compose(add1, dbl)(3));
let memoFib = (function () {
    let memo = {};
    function f(n) {
        if (n < 2) { return n; }
        if (memo[n] !== undefined) { return memo[n]; }
        let v = f(n - 1) + f(n - 2);
        memo[n] = v;
        return v;
    }
    return f;
})();
print("memofib", memoFib(60));
function adder(base) {
    return (x) => base + x;
}
let add5 = adder(5);
let add10 = adder(10);
print("curry", add5(3), add10(3), add5(add10(2)));
function applyTwice(fn, v) { return fn(fn(v)); }
print("twice", applyTwice((x) => x * 3, 2));
