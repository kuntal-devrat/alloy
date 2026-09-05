// t02: strings — concat, template literals with nesting, char access, length,
// relational comparison (lexicographic for string/string, numeric otherwise).
let s = "Hello";
let t = 'World';
let full = s + ", " + t + "!";
print(full);
print(full.length, full[0], full[11], full[99]);
print("abc" < "abd", "b" > "a", "aa" <= "aa", "a" < "aa");
print("10" < "9", "2" > "12", "abc" === "abc", "abc" !== "abd");
print("5" < 6, 1 < "2", "10" < 9, "10" > 9);
let n = 42;
let tmpl = `val=${n} sum=${1 + 2} nested=${`${true ? "y" : "n"}`} end`;
print(tmpl);
let chars = "";
for (let c of "abc") { chars = chars + c + ";"; }
print(chars);
let rev = "";
for (let i = s.length - 1; i >= 0; i--) { rev = rev + s[i]; }
print(rev);
let keys = "";
for (let k in { a: 1, b: 2, c: 3 }) { keys = keys + k; }
print(keys);
let quoted = "it's " + 'say "hi"' + " done";
print(quoted);
print("empty", "".length, ""[0], "" < "a");
