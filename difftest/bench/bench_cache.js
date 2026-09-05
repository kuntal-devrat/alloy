// Cache-server style: a hot Map keyed by generated IDs, holding per-key
// objects, with eviction churn. Hash-heavy — the PRD cache-server scenario.
// Deterministic; outputs verified against Node.
let cache = new Map();
let hits = 0, misses = 0;
let total = 0;
for (let round = 0; round < 40; round++) {
    for (let i = 0; i < 2500; i++) {
        let key = "user_" + (i % 1500) + "_r" + (round % 7);
        let v = cache.get(key);
        if (v === undefined) {
            cache.set(key, { id: i, visits: 1 });
            misses++;
        } else {
            v.visits++;
            hits++;
            total += v.visits;
        }
    }
    if (round % 10 === 9) {
        // Eviction burst: clear the whole cache twice per run.
        cache.clear();
    }
}
print(cache.size, hits, misses, total);
