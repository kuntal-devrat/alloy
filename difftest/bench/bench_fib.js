// bench: recursive fibonacci (call overhead) + memoized
function fib(n) {
    if (n < 2) { return n; }
    return fib(n - 1) + fib(n - 2);
}
let r = fib(30);
function memoFib(n) {
    let memo = [];
    function f(k) {
        if (k < 2) { return k; }
        if (memo[k] !== undefined) { return memo[k]; }
        let v = f(k - 1) + f(k - 2);
        memo[k] = v;
        return v;
    }
    return f(n);
}
let m = 0;
for (let i = 0; i < 2000; i++) { m += memoFib(77) % 1000; }
print(r, m);
