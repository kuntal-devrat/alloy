// bench: n-queens backtracking — recursion with loop-based conflict checks
let solutions = 0;
function nqueens(n) {
    let cols = [];
    for (let i = 0; i < n; i++) { cols[i] = -1; }
    function place(row) {
        if (row === n) { solutions += 1; return; }
        for (let col = 0; col < n; col++) {
            let ok = true;
            for (let r = 0; r < row; r++) {
                if (cols[r] === col || cols[r] - col === r - row || cols[r] - col === row - r) {
                    ok = false;
                    break;
                }
            }
            if (ok) {
                cols[row] = col;
                place(row + 1);
            }
        }
    }
    place(0);
}
nqueens(9);
print(solutions);
