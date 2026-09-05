// t17: graph algorithms — DFS, BFS distances, connected components.
function buildGraph() {
    let g = [];
    g[0] = [1, 5];
    g[1] = [0, 2];
    g[2] = [1, 3];
    g[3] = [2, 4];
    g[4] = [3];
    g[5] = [0, 6];
    g[6] = [5];
    return g;
}
function dfs(g, start, target) {
    let seen = [];
    for (let i = 0; i < g.length; i++) { seen[i] = false; }
    let found = false;
    function visit(v) {
        if (found) { return; }
        seen[v] = true;
        if (v === target) { found = true; return; }
        for (let i = 0; i < g[v].length; i++) {
            let nb = g[v][i];
            if (!seen[nb]) { visit(nb); }
        }
    }
    visit(start);
    return found;
}
function bfs(g, start) {
    let dist = [];
    for (let i = 0; i < g.length; i++) { dist[i] = -1; }
    dist[start] = 0;
    let queue = [start];
    let head = 0;
    while (head < queue.length) {
        let v = queue[head];
        head += 1;
        for (let i = 0; i < g[v].length; i++) {
            let nb = g[v][i];
            if (dist[nb] === -1) {
                dist[nb] = dist[v] + 1;
                queue[queue.length] = nb;
            }
        }
    }
    return dist;
}
let g = buildGraph();
print("dfs", dfs(g, 0, 4), dfs(g, 0, 6), dfs(g, 1, 5), dfs(g, 0, 7));
let d = bfs(g, 0);
print("bfs", d[0], d[1], d[4], d[5], d[6]);
function countComponents(n, edges) {
    let adj = [];
    for (let i = 0; i < n; i++) { adj[i] = []; }
    for (let i = 0; i < edges.length; i++) {
        let a = edges[i][0];
        let b = edges[i][1];
        adj[a][adj[a].length] = b;
        adj[b][adj[b].length] = a;
    }
    let seen = [];
    for (let i = 0; i < n; i++) { seen[i] = false; }
    function explore(v) {
        for (let i = 0; i < adj[v].length; i++) {
            let nb = adj[v][i];
            if (!seen[nb]) {
                seen[nb] = true;
                explore(nb);
            }
        }
    }
    let comps = 0;
    for (let i = 0; i < n; i++) {
        if (!seen[i]) {
            comps += 1;
            seen[i] = true;
            explore(i);
        }
    }
    return comps;
}
print("components", countComponents(7, [[0, 1], [1, 2], [3, 4], [5, 6]]));
print("components2", countComponents(5, []));
print("components3", countComponents(4, [[0, 1], [1, 2], [2, 3], [3, 0]]));
function shortestPath(g, start, end) {
    let prev = [];
    for (let i = 0; i < g.length; i++) { prev[i] = -1; }
    let queue = [start];
    let head = 0;
    prev[start] = start;
    while (head < queue.length) {
        let v = queue[head];
        head += 1;
        if (v === end) { break; }
        for (let i = 0; i < g[v].length; i++) {
            let nb = g[v][i];
            if (prev[nb] === -1) {
                prev[nb] = v;
                queue[queue.length] = nb;
            }
        }
    }
    let path = [];
    let cur = end;
    while (cur !== start) {
        path[path.length] = cur;
        cur = prev[cur];
    }
    path[path.length] = start;
    return path.length - 1;
}
print("shortest", shortestPath(g, 0, 4), shortestPath(g, 0, 6), shortestPath(g, 5, 3));
