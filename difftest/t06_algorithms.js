// t06: classic algorithms — sieve, collatz chains, gcd/lcm, factorials.
function sieve(n) {
    let prime = [];
    for (let i = 0; i <= n; i++) { prime[i] = true; }
    prime[0] = false;
    prime[1] = false;
    for (let i = 2; i * i <= n; i++) {
        if (prime[i]) {
            for (let j = i * i; j <= n; j += i) { prime[j] = false; }
        }
    }
    let count = 0;
    let largest = 0;
    for (let i = 2; i <= n; i++) {
        if (prime[i]) {
            count += 1;
            largest = i;
        }
    }
    return [count, largest];
}
let p = sieve(100);
print("primes", p[0], p[1]);
let p2 = sieve(1000);
print("primes-1000", p2[0], p2[1]);
function collatz(n) {
    let steps = 0;
    while (n !== 1) {
        if (n % 2 === 0) { n = n / 2; }
        else { n = 3 * n + 1; }
        steps += 1;
    }
    return steps;
}
let best = 0;
let bestN = 0;
for (let i = 1; i < 1000; i++) {
    let s = collatz(i);
    if (s > best) { best = s; bestN = i; }
}
print("collatz", bestN, best);
function gcd(a, b) {
    while (b !== 0) {
        let t = a % b;
        a = b;
        b = t;
    }
    return a;
}
function lcm(a, b) { return a / gcd(a, b) * b; }
print("gcd", gcd(48, 36), gcd(17, 5), gcd(1071, 462));
print("lcm", lcm(4, 6), lcm(21, 6));
let facts = "";
for (let i = 1; i <= 10; i++) {
    let f = 1;
    for (let j = 2; j <= i; j++) { f *= j; }
    facts = facts + f + " ";
}
print("facts", facts);
function sumDigits(n) {
    let total = 0;
    while (n > 0) {
        total += n % 10;
        n = n / 10 - (n % 10) / 10;
    }
    return total;
}
print("sumdigits", sumDigits(12345), sumDigits(999));
