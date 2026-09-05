// t16: recursive-descent expression evaluator — parsing strings with closure
// state (shared `pos` upvalue), precedence, and parenthesization.
function evaluate(s) {
    let pos = 0;
    let digits = { "0": 0, "1": 1, "2": 2, "3": 3, "4": 4, "5": 5, "6": 6, "7": 7, "8": 8, "9": 9 };
    function peek() { return s[pos]; }
    function isDigit(c) { return c !== undefined && digits[c] !== undefined; }
    function parseFactor() {
        let c = peek();
        if (c === "(") {
            pos += 1;
            let v = parseExpr();
            pos += 1;
            return v;
        }
        if (isDigit(c)) {
            pos += 1;
            return digits[c];
        }
        return 0;
    }
    function parseTerm() {
        let v = parseFactor();
        while (true) {
            let c = peek();
            if (c === "*") { pos += 1; v = v * parseFactor(); }
            else if (c === "/") { pos += 1; v = v / parseFactor(); }
            else { break; }
        }
        return v;
    }
    function parseExpr() {
        let v = parseTerm();
        while (true) {
            let c = peek();
            if (c === "+") { pos += 1; v = v + parseTerm(); }
            else if (c === "-") { pos += 1; v = v - parseTerm(); }
            else { break; }
        }
        return v;
    }
    return parseExpr();
}
print(evaluate("1+2*3"));
print(evaluate("(1+2)*3"));
print(evaluate("10-4/2"));
print(evaluate("2*(3+4)-5"));
print(evaluate("7"));
print(evaluate("((2+3))"));
print(evaluate("1+2+3+4+5"));
print(evaluate("8/2/2"));
print(evaluate("9-2-3"));
print(evaluate("(1+2)*(3+4)"));
print(evaluate("2*3*4/6"));
print(evaluate("100-99"));
