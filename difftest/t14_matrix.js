// t14: arrays and objects — matrix multiply, transpose, pascal, indexing.
function matMul(a, b) {
    let n = a.length;
    let out = [];
    for (let i = 0; i < n; i++) {
        out[i] = [];
        for (let j = 0; j < n; j++) {
            let s = 0;
            for (let k = 0; k < n; k++) { s += a[i][k] * b[k][j]; }
            out[i][j] = s;
        }
    }
    return out;
}
let m1 = [[1, 2], [3, 4]];
let m2 = [[5, 6], [7, 8]];
let m3 = matMul(m1, m2);
print("matmul", m3[0][0], m3[0][1], m3[1][0], m3[1][1]);
let rows = 6;
let pascal = [];
for (let i = 0; i < rows; i++) {
    pascal[i] = [];
    pascal[i][0] = 1;
    for (let j = 1; j <= i; j++) {
        if (j === i) { pascal[i][j] = 1; }
        else { pascal[i][j] = pascal[i - 1][j - 1] + pascal[i - 1][j]; }
    }
}
print("pascal", pascal[5][0], pascal[5][2], pascal[5][5]);
let trace = 0;
for (let i = 0; i < 2; i++) { trace += m1[i][i]; }
print("trace", trace);
function transpose(m) {
    let out = [];
    for (let j = 0; j < m[0].length; j++) {
        out[j] = [];
        for (let i = 0; i < m.length; i++) { out[j][i] = m[i][j]; }
    }
    return out;
}
let mt = transpose(m1);
print("transpose", mt[0][1], mt[1][0]);
let grid = [];
for (let i = 0; i < 3; i++) {
    grid[i] = [];
    for (let j = 0; j < 3; j++) { grid[i][j] = i * 3 + j; }
}
let diag = grid[0][0] + grid[1][1] + grid[2][2];
print("grid", grid[2][2], diag);
let counts = {};
for (let i = 0; i < 20; i++) {
    let k = "k" + (i % 4);
    if (counts[k] === undefined) { counts[k] = 0; }
    counts[k] += 1;
}
print("counts", counts.k0, counts.k1, counts.k2, counts.k3);
let fibArr = [0, 1];
for (let i = 2; i < 20; i++) { fibArr[i] = fibArr[i - 1] + fibArr[i - 2]; }
print("fibarr", fibArr[19]);
let sum2d = 0;
for (let i = 0; i < grid.length; i++) {
    for (let j = 0; j < grid[i].length; j++) { sum2d += grid[i][j]; }
}
print("sum2d", sum2d);
let revArr = [];
let src = [1, 2, 3, 4, 5];
for (let i = src.length - 1; i >= 0; i--) { revArr[revArr.length] = src[i]; }
print("revarr", revArr[0], revArr[2], revArr[4]);
