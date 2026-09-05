// t05: sorting and searching — quicksort, insertion sort, binary search.
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
let data = [5, 3, 8, 1, 9, 2, 7, 4, 6, 0];
quicksort(data, 0, data.length - 1);
print("sorted", data[0], data[3], data[9]);
let desc = [9, 8, 7, 6, 5, 4, 3, 2, 1, 0];
quicksort(desc, 0, desc.length - 1);
print("desc-sorted", desc[0], desc[9]);
let dup = [3, 1, 3, 2, 3, 1, 2, 2, 1];
quicksort(dup, 0, dup.length - 1);
print("dup", dup[0], dup[4], dup[8]);
function insertionSort(arr) {
    for (let i = 1; i < arr.length; i++) {
        let key = arr[i];
        let j = i - 1;
        while (j >= 0 && arr[j] > key) {
            arr[j + 1] = arr[j];
            j -= 1;
        }
        arr[j + 1] = key;
    }
}
let d2 = [9, 4, 7, 2, 8, 1, 5, 3, 6];
insertionSort(d2);
print("insort", d2[0], d2[4], d2[8]);
function bsearch(arr, x) {
    let lo = 0;
    let hi = arr.length - 1;
    while (lo <= hi) {
        let mid = (lo + hi - (lo + hi) % 2) / 2;
        if (arr[mid] === x) { return mid; }
        if (arr[mid] < x) { lo = mid + 1; }
        else { hi = mid - 1; }
    }
    return -1;
}
let sorted = [1, 3, 5, 7, 9, 11, 13];
print("bsearch", bsearch(sorted, 7), bsearch(sorted, 4), bsearch(sorted, 13), bsearch(sorted, 0));
