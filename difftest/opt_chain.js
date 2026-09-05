// Optional chaining differential — every shape
let o = { a: { b: 42 }, m: function(x, y) { return this.a.b + x + y; }, arr: [10, 20, 30], n: null, z: 0, s: "hi" };
let undef = undefined;

// prop access
console.log(o?.a.b);                 // 42
console.log(o?.x?.y);                // undefined
console.log(undef?.a);               // undefined
console.log(o?.n?.b);                // undefined
console.log(o?.z);                   // 0

// index access
console.log(o?.["a"]?.b);            // 42
console.log(o?.arr?.[1]);            // 20
console.log(undef?.[0]);             // undefined
console.log(o?.n?.[0]);              // undefined

// calls
function f(x, y) { return x * 10 + y; }
console.log(f?.(3, 4));              // 34
let fnull = null;
console.log(fnull?.(3, 4));          // undefined
console.log(undef?.(1));             // undefined

// method calls with this binding
console.log(o.m?.(1, 2));            // 45
let o2 = { n: null };
console.log(o2?.m?.(1));             // undefined (m undefined on o2? no — o2 has no m: undefined?.(1))
console.log(o?.n?.m?.(1));           // undefined
console.log(o?.m?.(1));              // o.m exists: 43

// short-circuit: args must NOT evaluate when receiver nullish
let calls = 0;
function bump() { calls++; return 5; }
console.log(null?.m?.(bump()));      // undefined, bump NOT called
console.log(calls);                  // 0
console.log(o?.m?.(bump()));         // 48 (this.a.b=42 + 5 + 1)
console.log(calls);                  // 1

// chained plain call then member
function g() { return { p: 7 }; }
console.log(g?.().p);                // 7
let gnull = null;
console.log(gnull?.().p);            // undefined
console.log(gnull?.()?.p);           // undefined

// member then call then member
console.log(o?.arr?.[1]?.toString?.());  // "20"
console.log(undef?.arr?.[1]);            // undefined

// spread in optional call
function sum(...xs) { let t = 0; for (let x of xs) { t += x; } return t; }
console.log(sum?.(1, 2, 3));         // 6
console.log(sum?.(...[1, 2, 3]));    // 6
let snull = null;
console.log(snull?.(...[1, 2]));     // undefined

// nested optional chains in expressions

// assignment target must NOT be optional (compile error) — not tested here

// statement-position chain discards cleanly
o?.a?.b;
undef?.a?.b;
gnull?.().p;
console.log("after-discard");        // marker
