// bench: string building (concatenation), indexing, and reversal
let alpha = "abcdefghijklmnopqrstuvwxyz";
let s = "";
for (let i = 0; i < 15000; i++) { s = s + alpha[i % 26]; }
let len = s.length;
let rev = "";
for (let i = len - 1; i >= 0; i--) { rev = rev + s[i]; }
let total = 0;
for (let i = 0; i < 20000; i++) {
    let c = s[i % len];
    if (c === "a") { total += 1; }
}
print(len, rev[0], rev[len - 1], total);
