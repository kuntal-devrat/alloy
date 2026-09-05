// bench: mixed string+number building (the `s = s + i` accumulator pattern)
let s = "";
for (let i = 0; i < 60000; i++) { s = s + i; }
let len = s.length;
let c = s[0] + s[len - 1];
let t = "";
for (let i = 0; i < 30000; i++) { t = t + (i % 10); }
print(len, c, t[0], t[29999]);
