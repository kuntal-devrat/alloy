// bench: matrix multiplication (nested loops + float arithmetic)
function matMul(a, b) {
    let n = a.length;
    let out = [];
    for (let i = 0; i < n; i++) {
        out[i] = [];
        for (let j = 0; j < n; j++) {
            let acc = 0;
            for (let k = 0; k < n; k++) { acc += a[i][k] * b[k][j]; }
            out[i][j] = acc;
        }
    }
    return out;
}
let size = 60;
let a = [];
let b = [];
for (let i = 0; i < size; i++) {
    a[i] = [];
    b[i] = [];
    for (let j = 0; j < size; j++) {
        a[i][j] = (i * size + j) % 101 / 10;
        b[i][j] = (j * size + i) % 97 / 10;
    }
}
let c = matMul(a, b);
let trace = 0;
for (let i = 0; i < size; i++) { trace += c[i][i]; }
let d = matMul(b, a);
let trace2 = 0;
for (let i = 0; i < size; i++) { trace2 += d[i][i]; }
// Both engines print the same f64 bits (shortest round-trip), so raw works.
print(trace, trace2);
