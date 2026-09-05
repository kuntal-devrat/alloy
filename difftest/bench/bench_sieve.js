// bench: sieve of Eratosthenes (array writes + nested loops)
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
    let sum = 0;
    for (let i = 2; i <= n; i++) {
        if (prime[i]) { count += 1; sum += i; }
    }
    return [count, sum % 1000000];
}
let r = sieve(200000);
let total = 0;
for (let i = 0; i < 4; i++) { total += sieve(50000)[0]; }
print(r[0], r[1], total);
