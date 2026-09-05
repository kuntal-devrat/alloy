// t09: spread and rest — rest params, array/call/string spread, copies.
function sum(...nums) {
    let t = 0;
    for (let n of nums) { t += n; }
    return t;
}
print("rest", sum(1, 2, 3), sum(1, 2, 3, 4, 5));
function mix(a, b, ...rest) {
    return a + b + rest.length;
}
print("mix", mix(1, 2), mix(1, 2, 3, 4, 5));
function first(...args) { return args[0]; }
print("first", first(9, 8, 7), first());
let arr = [1, 2, 3];
let spread = [0, ...arr, 4];
print("spread-arr", spread.length, spread[0], spread[3]);
function add3(a, b, c) { return a + b + c; }
let args = [10, 20, 30];
print("spread-call", add3(...args));
print("spread-mixed", add3(1, ...[2, 3]));
let chars = [..."ab"];
print("spread-str", chars.length, chars[0], chars[1]);
let nested = [...[1, ...[2, 3]]];
print("nested-spread", nested.length, nested[0], nested[2]);
let [fst, ...others] = [7, 8, 9, 10];
print("rest-elem2", fst, others.length, others[2]);
let copy = [...arr];
copy[0] = 99;
print("copy-indep", arr[0], copy[0]);
let zero = [];
print("empty-spread", sum(...zero));
let mid = [1, ...[2, 3, 4], 5];
print("mid-spread", mid.length, mid[0], mid[3], mid[4]);
let concat = [...arr, ...spread];
print("concat-spread", concat.length, concat[5]);
function collect(label, ...vals) {
    let s = label + ":";
    for (let v of vals) { s = s + v; }
    return s;
}
print("collect", collect("sum", 1, 2, 3), collect("none"));
