// bench: collatz — while loops and mixed arithmetic over a large range
function collatzSteps(n) {
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
let total = 0;
for (let i = 1; i < 60000; i++) {
    let s = collatzSteps(i);
    total += s;
    if (s > best) { best = s; bestN = i; }
}
print(bestN, best, total);
