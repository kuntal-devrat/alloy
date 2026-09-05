// bench: array churn — LCG fill, insertion sort, binary search
let seed = 12345;
function rnd() {
    seed = (seed * 48271) % 2147483648;
    return seed;
}
let n = 60000;
let arr = [];
for (let i = 0; i < n; i++) { arr[i] = rnd() % 100000; }
let sum = 0;
for (let i = 0; i < n; i++) { sum += arr[i] % 10; }
function insertionSort(a) {
    for (let i = 1; i < a.length; i++) {
        let key = a[i];
        let j = i - 1;
        while (j >= 0 && a[j] > key) {
            a[j + 1] = a[j];
            j -= 1;
        }
        a[j + 1] = key;
    }
}
let small = [];
for (let i = 0; i < 4000; i++) { small[i] = rnd() % 50000; }
insertionSort(small);
let sortedCheck = 0;
for (let i = 1; i < small.length; i++) {
    if (small[i - 1] <= small[i]) { sortedCheck += 1; }
}
function bsearch(a, x) {
    let lo = 0;
    let hi = a.length - 1;
    while (lo <= hi) {
        let mid = (lo + hi - (lo + hi) % 2) / 2;
        if (a[mid] === x) { return mid; }
        if (a[mid] < x) { lo = mid + 1; }
        else { hi = mid - 1; }
    }
    return -1;
}
let found = 0;
for (let i = 0; i < 30000; i++) {
    if (bsearch(small, rnd() % 50000) >= 0) { found += 1; }
}
print(sum % 1000, sortedCheck, found);
