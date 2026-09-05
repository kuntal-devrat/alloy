// t13: heavy recursion — ackermann, hanoi, n-queens, permutations.
function ack(m, n) {
    if (m === 0) { return n + 1; }
    if (n === 0) { return ack(m - 1, 1); }
    return ack(m - 1, ack(m, n - 1));
}
print("ack", ack(2, 3), ack(3, 3));
function hanoi(n, src, dst, aux) {
    if (n === 0) { return 0; }
    let moves = hanoi(n - 1, src, aux, dst);
    moves += 1;
    moves += hanoi(n - 1, aux, dst, src);
    return moves;
}
print("hanoi", hanoi(10, "a", "c", "b"));
let queens = 0;
function nqueens(n) {
    let cols = [];
    for (let i = 0; i < n; i++) { cols[i] = -1; }
    function place(row) {
        if (row === n) { queens += 1; return; }
        for (let c = 0; c < n; c++) {
            let ok = true;
            for (let r = 0; r < row; r++) {
                if (cols[r] === c || cols[r] - c === r - row || cols[r] - c === row - r) {
                    ok = false;
                    break;
                }
            }
            if (ok) {
                cols[row] = c;
                place(row + 1);
            }
        }
    }
    place(0);
}
nqueens(8);
print("queens", queens);
function perms(arr, k) {
    if (k === arr.length) { return 1; }
    let total = 0;
    for (let i = k; i < arr.length; i++) {
        let t = arr[k];
        arr[k] = arr[i];
        arr[i] = t;
        total += perms(arr, k + 1);
        t = arr[k];
        arr[k] = arr[i];
        arr[i] = t;
    }
    return total;
}
print("perms", perms([1, 2, 3, 4, 5, 6], 0));
function gcdRec(a, b) {
    if (b === 0) { return a; }
    return gcdRec(b, a % b);
}
print("gcd-rec", gcdRec(1071, 462));
function countPaths(m, n) {
    if (m === 0 || n === 0) { return 1; }
    return countPaths(m - 1, n) + countPaths(m, n - 1);
}
print("paths", countPaths(3, 3));
function treeSum(node) {
    if (node === null) { return 0; }
    return node.v + treeSum(node.left) + treeSum(node.right);
}
let tree = {
    v: 1,
    left: { v: 2, left: { v: 4, left: null, right: null }, right: { v: 5, left: null, right: null } },
    right: { v: 3, left: null, right: null }
};
print("tree-sum", treeSum(tree));
