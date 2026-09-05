// bench: quicksort — recursion, partition swaps, comparisons
function partition(arr, lo, hi) {
    let pivot = arr[hi];
    let i = lo - 1;
    for (let j = lo; j < hi; j++) {
        if (arr[j] < pivot) {
            i += 1;
            let tmp = arr[i];
            arr[i] = arr[j];
            arr[j] = tmp;
        }
    }
    let tmp = arr[i + 1];
    arr[i + 1] = arr[hi];
    arr[hi] = tmp;
    return i + 1;
}
function quicksort(arr, lo, hi) {
    if (lo < hi) {
        let p = partition(arr, lo, hi);
        quicksort(arr, lo, p - 1);
        quicksort(arr, p + 1, hi);
    }
}
let seed = 98765;
function rnd() {
    seed = (seed * 48271) % 2147483648;
    return seed;
}
let n = 50000;
let arr = [];
for (let i = 0; i < n; i++) { arr[i] = rnd() % 1000000; }
quicksort(arr, 0, n - 1);
let checks = 0;
let sorted = 1;
for (let i = 1; i < n; i++) {
    if (arr[i - 1] <= arr[i]) { checks += 1; }
}
for (let i = 0; i < 10; i++) { sorted += arr[i * 5000]; }
print(checks, sorted);
