// bench: tight integer loops (sum, nested, modulo/division mix)
let s = 0;
for (let i = 1; i <= 2000000; i++) { s += i; }
let nested = 0;
for (let i = 0; i < 1000; i++) {
    for (let j = 0; j < 1000; j++) { nested += (i + j) % 7; }
}
let mix = 0;
for (let i = 1; i <= 800000; i++) { mix += (i * 3) % 1000 + i / 2 - (i % 5); }
print(s, nested, mix);
