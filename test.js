print(10 + 3)
print(10 - 3)
print(10 * 3)
print(10 / 3)
print(10 % 3)
print("Hello World!")
print(true && false)
print(true || false)
print(!true)
print(10 > 3)
print(10 < 3)
print(10 == 10)
print(10 != 3)

let x = 5
if x > 3 {
    print("a is greater")
} else {
    print("a is not greater")
}

let y = 2
if y > 3 {
    print("b is greater")
} else {
    print("b is not greater")
}

let i = 0
let sum = 0
while i < 5 {
    sum = sum + i
    i = i + 1
}
print("sum 0..4 =", sum)

let fact = 1
let n = 5
for let f = 1; f <= n; f = f + 1 {
    fact = fact * f
}
print("5! =", fact)

print([10, 20, 30, 40, 50])
print((2 + 3) * 4)

let c = 0
let count = 0
while c < 3 {
    count = count + 1
    c = c + 1
}
print("counter =", count)

print("Welcome to alloy runtime!")
