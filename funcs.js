function add(a, b) {
    return a + b
}
print(add(2, 3))

function fib(n) {
    if n <= 1 {
        return n
    }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))

const square = function(x) {
    return x * x
}
print(square(5))

function apply(f, x) {
    return f(x)
}
print(apply(square, 6))

function fact(n) {
    if n <= 1 {
        return 1
    }
    return n * fact(n - 1)
}
print(fact(5))

const obj = { label: "hello", count: 42 }
print(obj.label)
print(obj.count)
print(obj.label, obj.count)
