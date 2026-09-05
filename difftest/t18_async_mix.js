// t18: async programs — pipelines with closures, rejected promises caught via
// await + try/catch, finally ordering, sequential chains.
async function double(x) {
    return x * 2;
}
async function fetchData(id) {
    if (id < 0) { throw "bad-id:" + id; }
    return { id: id, value: id * 10 };
}
async function processItem(item) {
    let doubled = await double(item.value);
    return doubled + item.id;
}
async function main() {
    let results = [];
    let errors = [];
    for (let i = 0; i < 5; i++) {
        try {
            let item = await fetchData(i - 1);
            let r = await processItem(item);
            results[results.length] = r;
        } catch (e) {
            errors[errors.length] = e;
        }
    }
    print("results", results.length, results[0], results[2]);
    print("errors", errors.length, errors[0]);
    let total = 0;
    async function accumulate(v) {
        total += v;
        await double(1);
        return total;
    }
    let a = await accumulate(5);
    let b = await accumulate(10);
    print("accum", a, b);
    let chain = "";
    async function step(label) {
        chain += label;
        await double(1);
        return label;
    }
    await step("x");
    await step("y");
    await step("z");
    print("chain", chain);
    let handled = "";
    try {
        try {
            await fetchData(-5);
        } catch (e) {
            handled += "inner";
            throw "rethrow";
        } finally {
            handled += "-fin";
        }
    } catch (e2) {
        handled += "-outer:" + e2;
    }
    print("handled", handled);
    let seq = 0;
    for (let i = 1; i <= 3; i++) {
        seq += await double(i);
    }
    print("seq", seq);
    async function nested() {
        let inner = await double(3);
        let outer2 = await double(inner);
        return outer2;
    }
    print("nested-call", await nested());
}
main();
