// t07: switch/case/default — dispatch, shared labels, fall-through, default
// position, nested switches, and continue/break inside loops.
function describe(n) {
    switch (n) {
        case 1:
            return "one";
        case 2:
        case 3:
            return "two-or-three";
        default:
            return "many";
    }
}
print(describe(1), describe(2), describe(3), describe(9));
let out = "";
function classify(x) {
    switch (x) {
        case "a":
            out += "A";
        case "b":
            out += "B";
            break;
        case "c":
            out += "C";
            break;
        default:
            out += "?";
    }
}
classify("a");
classify("b");
classify("c");
classify("z");
print("fallthrough", out);
let total = 0;
for (let i = 0; i < 5; i++) {
    switch (i) {
        case 0:
            total += 1;
            continue;
        case 2:
            total += 10;
            break;
        default:
            total += 2;
    }
}
print("switch-loop", total);
let sw = 1;
switch (sw) {
    case 1: {
        let inside = 5;
        sw = inside * 2;
        break;
    }
}
print("switch-block", sw);
let nested = 2;
let result = "";
switch (nested) {
    case 1:
        result += "one";
        break;
    case 2:
        switch (nested) {
            case 2:
                result += "two";
                break;
            default:
                result += "inner-default";
        }
        result += "-outer";
        break;
    default:
        result += "outer-default";
}
print("nested-switch", result);
// Strict equality dispatch: no coercion between arms.
let typed = "";
switch (1) {
    case "1":
        typed += "string";
        break;
    case 1:
        typed += "number";
        break;
    default:
        typed += "none";
}
print("typed-switch", typed);
// No match, no default: nothing runs.
let before = 7;
switch (99) {
    case 1:
        before = 1;
        break;
    case 2:
        before = 2;
}
print("no-match", before);
