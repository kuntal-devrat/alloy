// t08: destructuring — declarations, renames, nesting, holes, rest elements,
// assignments (incl. swap), for-of headers, strings.
let { a, b } = { a: 1, b: 2 };
print(a, b);
let [x, y] = [10, 20];
print(x, y);
let { p: renamed, q: { r } } = { p: 7, q: { r: 8 } };
print(renamed, r);
let [m, , n2] = [1, 2, 3];
print("holes", m, n2);
let [head, ...tail] = [4, 5, 6, 7];
print("rest-elem", head, tail.length, tail[0], tail[2]);
let [c1, c2, ...cs] = "hello";
print("str-destructure", c1, c2, cs.length, cs[0]);
let v1 = 1;
let v2 = 2;
[v1, v2] = [v2, v1];
print("swap", v1, v2);
let t1 = 0;
let t2 = 0;
let t3 = 0;
[t1, , t3] = [9, 99, 8];
print("assign-hole", t1, t3);
let obj = { name: "x", value: 3 };
let { name: nName, value: nVal } = obj;
print("rename2", nName, nVal);
let pairs = [[1, "one"], [2, "two"]];
let acc = "";
for (let [num, word] of pairs) { acc = acc + num + word; }
print("forof-destructure", acc);
let objs = [{ k: "a", v: 1 }, { k: "b", v: 2 }];
let acc2 = "";
for (let { k, v } of objs) { acc2 = acc2 + k + v; }
print("forof-obj", acc2);
let [deep, [d1, ...drest]] = [1, [2, 3, 4]];
print("nested-rest", deep, d1, drest.length, drest[1]);
let [x0, x1, x2] = [5, 6, 7];
[x0, x1, x2] = [x2, x0, x1];
print("rotate", x0, x1, x2);
let { missing } = {};
print("missing", missing);
