// t10: exceptions — throw/catch/finally, finally on break/return, rethrow,
// nested try, loop + try interplay. All exceptions are caught.
function risky(n) {
    if (n === 0) { throw "zero!"; }
    if (n === 1) { throw { code: 42, msg: "one!" }; }
    return n * 2;
}
let r1 = "";
try {
    r1 = risky(2);
} catch (e) {
    r1 = "caught:" + e;
}
print("no-throw", r1);
try {
    risky(0);
} catch (e) {
    print("caught-str", e);
}
try {
    risky(1);
} catch (e) {
    print("caught-obj", e.code, e.msg);
}
let f = "";
try {
    try {
        throw "inner";
    } finally {
        f += "F1";
    }
} catch (e) {
    f += "C";
}
print("finally-catch", f);
let out = "";
for (let i = 0; i < 3; i++) {
    try {
        if (i === 1) { throw "skip"; }
        out += i;
    } catch (e) {
        out += "E";
    } finally {
        out += "F";
    }
}
print("loop-try", out);
let breaks = "";
for (let i = 0; i < 5; i++) {
    try {
        if (i === 2) { break; }
        breaks += i;
    } finally {
        breaks += ".";
    }
}
print("break-finally", breaks);
let conts = "";
for (let i = 0; i < 4; i++) {
    try {
        if (i % 2 === 0) { continue; }
        conts += i;
    } finally {
        conts += ",";
    }
}
print("continue-finally", conts);
let ret = "";
function finRet() {
    try {
        return "body";
    } finally {
        ret += "cleanup";
    }
}
print("return-finally", finRet(), ret);
let nest = "";
try {
    try {
        throw "deep";
    } catch (e) {
        nest += "inner:" + e;
        throw "rethrow";
    }
} catch (e2) {
    nest += " outer:" + e2;
}
print("rethrow", nest);
let finOrder = "";
function nestedFinally() {
    try {
        try {
            return "v";
        } finally {
            finOrder += "in";
        }
    } finally {
        finOrder += "out";
    }
}
print("nested-finally", nestedFinally(), finOrder);
let val = 0;
try {
    val = risky(5);
} finally {
    val += 1;
}
print("finally-after", val);
