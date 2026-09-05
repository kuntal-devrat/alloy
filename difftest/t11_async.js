// t11: async/await — value chains, Promise.resolve, Promise.withResolvers,
// rejection via await + try/catch, ordering, finally around await.
async function double(x) {
    return x * 2;
}
async function main() {
    let a = await double(21);
    print("await", a);
    let b = await Promise.resolve(100);
    print("promise-resolve", b);
    let c = await 7;
    print("await-literal", c);
    let wr = Promise.withResolvers();
    wr.resolve(5);
    let e = await wr.promise;
    print("withResolvers", e);
    let err = "";
    try {
        let w2 = Promise.withResolvers();
        w2.reject("boom");
        await w2.promise;
    } catch (ex) {
        err = "caught:" + ex;
    }
    print("reject-caught", err);
    let order = "";
    async function inner() {
        order += "in";
        return "inner-val";
    }
    order += "a";
    let f = await inner();
    order += "b";
    print("async-order", f, order);
    let seq = "";
    seq += "1";
    await double(1);
    seq += "2";
    await double(2);
    seq += "3";
    print("await-seq", seq);
    let chained = await double(await double(3));
    print("chained-await", chained);
    let fin = "";
    async function withFinally() {
        try {
            return await double(4);
        } finally {
            fin += "done";
        }
    }
    let g = await withFinally();
    print("await-finally", g, fin);
    let deep = await double(await double(await double(2)));
    print("deep-await", deep);
}
main();
