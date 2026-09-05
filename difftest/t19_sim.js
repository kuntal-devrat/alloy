// t19: simulation — Conway's Game of Life (heavy nested loops, non-exiting
// ifs) and a closure-based vending machine.
function stepLife(grid, w, h) {
    let next = [];
    for (let y = 0; y < h; y++) {
        next[y] = [];
        for (let x = 0; x < w; x++) {
            let live = 0;
            for (let dy = -1; dy <= 1; dy++) {
                for (let dx = -1; dx <= 1; dx++) {
                    if (dx === 0 && dy === 0) { continue; }
                    let nx = x + dx;
                    let ny = y + dy;
                    if (nx >= 0 && nx < w && ny >= 0 && ny < h) {
                        if (grid[ny][nx] === 1) { live += 1; }
                    }
                }
            }
            let cell = grid[y][x];
            if (cell === 1) {
                if (live === 2 || live === 3) { next[y][x] = 1; }
                else { next[y][x] = 0; }
            } else {
                if (live === 3) { next[y][x] = 1; }
                else { next[y][x] = 0; }
            }
        }
    }
    return next;
}
function countLive(grid) {
    let total = 0;
    for (let y = 0; y < grid.length; y++) {
        for (let x = 0; x < grid[y].length; x++) {
            if (grid[y][x] === 1) { total += 1; }
        }
    }
    return total;
}
// Blinker: a vertical line of 3 oscillates to horizontal and back.
let grid = [
    [0, 0, 0, 0, 0],
    [0, 0, 1, 0, 0],
    [0, 0, 1, 0, 0],
    [0, 0, 1, 0, 0],
    [0, 0, 0, 0, 0]
];
let g2 = stepLife(grid, 5, 5);
let g3 = stepLife(g2, 5, 5);
print("life", countLive(g2), countLive(g3), g2[1][2], g2[2][2], g2[3][2], g2[2][1]);
// Glider-ish block (still life): 2x2 square stays put.
let block = [
    [0, 0, 0, 0],
    [0, 1, 1, 0],
    [0, 1, 1, 0],
    [0, 0, 0, 0]
];
let b2 = stepLife(block, 4, 4);
print("block", countLive(b2));
function createVending() {
    let stock = { cola: 3, chips: 2 };
    let coins = 0;
    return {
        insert: (n) => { coins += n; return coins; },
        buy: (item) => {
            if (stock[item] === undefined) { throw "unknown:" + item; }
            if (coins < 2) { throw "need-more"; }
            if (stock[item] === 0) { throw "sold-out"; }
            stock[item] -= 1;
            coins -= 2;
            return coins;
        },
        stock: (item) => stock[item],
        refund: () => { let c = coins; coins = 0; return c; }
    };
}
let vm = createVending();
print("vending", vm.insert(5), vm.buy("cola"), vm.stock("cola"));
vm.insert(2);
vm.buy("chips");
vm.buy("cola");
print("vending2", vm.stock("chips"), vm.stock("cola"));
let verr = "";
try { vm.buy("cola"); } catch (e) { verr = e; }
print("vending-err", verr);
print("vending-refund", vm.refund());
let v2 = createVending();
let verr2 = "";
try { v2.buy("tea"); } catch (e) { verr2 = e; }
print("vending-unknown", verr2);
