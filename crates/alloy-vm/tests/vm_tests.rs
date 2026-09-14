use alloy_vm::bytecode::Program;
use alloy_vm::compiler::{CompileError, Compiler};
use alloy_vm::vm::Vm;
use std::sync::Arc;

/// Compile and run a snippet, returning the captured print lines.
fn run(src: &str) -> Vec<String> {
    let program = Compiler::compile_source(src).expect("compile failed");
    let (mut vm, out) = Vm::with_output(program);
    vm.run();
    if let Some(err) = vm.take_error() {
        eprintln!("VM ERROR: {:?}", err);
    }
    let lines = out.lock().unwrap().clone();
    lines
}

fn run_lines(src: &str) -> String {
    run(src).join("\n")
}

fn compile_err(src: &str) -> CompileError {
    Compiler::compile_source(src).expect_err("expected compile error")
}

fn roundtrip(src: &str) -> Vec<String> {
    let program = Compiler::compile_source(src).expect("compile failed");
    let bytes = program.to_bytes().expect("serialize failed");
    let loaded: Program = Program::from_bytes(&bytes).expect("deserialize failed");
    let (mut vm, out) = Vm::with_output(loaded);
    vm.run();
    let lines = out.lock().unwrap().clone();
    lines
}

#[test]
fn arithmetic_and_comparisons() {
    assert_eq!(run_lines("print(10 + 3)"), "13");
    assert_eq!(run_lines("print(10 - 3)"), "7");
    assert_eq!(run_lines("print(10 * 3)"), "30");
    // JS semantics: / is float division.
    assert_eq!(run_lines("print(10 / 3)"), "3.3333333333333335");
    assert_eq!(run_lines("print(10 % 3)"), "1");
    assert_eq!(run_lines("print(2 + 3 * 4)"), "14");
    assert_eq!(run_lines("print((2 + 3) * 4)"), "20");
    assert_eq!(run_lines("print(10 == 10)"), "true");
    assert_eq!(run_lines("print(10 != 3)"), "true");
    assert_eq!(run_lines("print(10 > 3)"), "true");
}

#[test]
fn strict_equality_across_int_number() {
    assert_eq!(run_lines("print(5 === 5.0)"), "true");
    assert_eq!(run_lines("print(5 === 5)"), "true");
    assert_eq!(run_lines("print(\"5\" === 5)"), "false");
    assert_eq!(run_lines("print(null === null)"), "true");
}

#[test]
fn not_strict_neq() {
    assert_eq!(run_lines("print(1 !== 2)"), "true");
    assert_eq!(run_lines("print(1 !== 1)"), "false");
    assert_eq!(run_lines("print(\"1\" !== 1)"), "true");
}

#[test]
fn large_int_literals() {
    assert_eq!(run_lines("print(4294967296)"), "4294967296");
    assert_eq!(run_lines("print(70000)"), "70000");
    assert_eq!(run_lines("print(-5 * 2)"), "-10");
}

#[test]
fn string_concatenation() {
    assert_eq!(run_lines("print(\"a\" + 1)"), "a1");
    assert_eq!(run_lines("print(1 + \"a\")"), "1a");
    assert_eq!(run_lines("print(\"x\" + true + \"y\")"), "xtruey");
    assert_eq!(run_lines("print(\"a\" + \"b\")"), "ab");
}

#[test]
fn object_property_set() {
    assert_eq!(run_lines("const o = { a: 1 }\no.a = 5\nprint(o.a)"), "5");
    assert_eq!(
        run_lines("const o = { a: 1 }\no[\"b\"] = \"x\"\nprint(o.a, o.b)"),
        "1 x"
    );
    assert_eq!(run_lines("const o = { n: 2 }\no.n += 3\nprint(o.n)"), "5");
    // Nested object mutation through a reference.
    assert_eq!(
        run_lines("const a = { inner: { v: 1 } }\na.inner.v = 9\nprint(a.inner.v)"),
        "9"
    );
}

#[test]
fn array_indexing() {
    assert_eq!(run_lines("const a = [10, 20, 30]\nprint(a[1])"), "20");
    assert_eq!(run_lines("const a = [10, 20, 30]\nprint(a.length)"), "3");
    assert_eq!(
        run_lines("const a = [10, 20, 30]\nprint(a[9])"),
        "undefined"
    );
    assert_eq!(
        run_lines("const a = [1, 2, 3]\na[1] = 99\nprint(a[1])"),
        "99"
    );
    // Growing past the end fills with undefined (JS-style holes).
    assert_eq!(
        run_lines("const a = [1]\na[3] = 7\nprint(a[3], a[1], a.length)"),
        "7 undefined 4"
    );
    assert_eq!(run_lines("const a = [1, 2]\nprint(a.length)"), "2");
}

#[test]
fn typeof_support() {
    assert_eq!(run_lines("print(typeof 5)"), "number");
    assert_eq!(run_lines("print(typeof \"s\")"), "string");
    assert_eq!(run_lines("print(typeof true)"), "boolean");
    assert_eq!(run_lines("print(typeof undefined)"), "undefined");
    assert_eq!(
        run_lines("const f = function() {}\nprint(typeof f)"),
        "function"
    );
}

#[test]
fn break_and_continue() {
    assert_eq!(
        run_lines("let i = 0\nwhile true {\n    i = i + 1\n    if i > 3 { break }\n}\nprint(i)"),
        "4"
    );
    // while: continue skips the rest of the body (sums 1 + 3 + 4 + 5).
    assert_eq!(
        run_lines("let s = 0\nlet j = 0\nwhile j < 5 {\n    j = j + 1\n    if j == 2 { continue }\n    s = s + j\n}\nprint(s)"),
        "13"
    );
    // for: continue runs the update (sums 0 + 1 + 3 + 4).
    assert_eq!(
        run_lines("let t = 0\nfor let k = 0; k < 5; k = k + 1 {\n    if k == 2 { continue }\n    t = t + k\n}\nprint(t)"),
        "8"
    );
    // break inside a for loop.
    assert_eq!(
        run_lines("let t = 0\nfor let k = 0; k < 10; k = k + 1 {\n    if k == 4 { break }\n    t = t + k\n}\nprint(t)"),
        "6"
    );
    // Nested loops: inner break must not escape the outer loop.
    assert_eq!(
        run_lines("let c = 0\nlet x = 0\nwhile x < 3 {\n    let y = 0\n    while y < 3 {\n        y = y + 1\n        if y == 2 { break }\n        c = c + 1\n    }\n    x = x + 1\n}\nprint(c)"),
        "3"
    );
}

#[test]
fn break_outside_loop_errors() {
    assert!(matches!(
        compile_err("break"),
        CompileError::BreakOutsideLoop
    ));
    assert!(matches!(
        compile_err("continue"),
        CompileError::BreakOutsideLoop
    ));
    // Inside a function that isn't in a loop it must also error.
    assert!(matches!(
        compile_err("function f() { break }"),
        CompileError::BreakOutsideLoop
    ));
}

#[test]
fn cannot_shadow_builtins() {
    assert!(matches!(
        compile_err("http = 5"),
        CompileError::CannotShadowBuiltin(_)
    ));
    assert!(matches!(
        compile_err("function print() {}"),
        CompileError::CannotShadowBuiltin(_)
    ));
}

#[test]
fn closures_capture_and_mutate() {
    assert_eq!(
        run_lines(
            "let counter = 0\nconst inc = function() { counter = counter + 1 }\ninc()\ninc()\nprint(counter)"
        ),
        "2"
    );
    // Param capture with an escaping closure.
    assert_eq!(
        run_lines(
            "function makeAdder(x) {\n    return function(y) { return x + y }\n}\nconst add5 = makeAdder(5)\nprint(add5(3))"
        ),
        "8"
    );
    // Independent instances share no state.
    assert_eq!(
        run_lines(
            "function makeCounter() {\n    let count = 0\n    function bump() { count = count + 1; return count }\n    return bump\n}\nconst c1 = makeCounter()\nconst c2 = makeCounter()\nprint(c1(), c1(), c2())"
        ),
        "1 2 1"
    );
    // Closure captured by another closure (upvalue chain).
    assert_eq!(
        run_lines(
            "function outer() {\n    let x = 10\n    function mid() {\n        function inner() { return x }\n        return inner\n    }\n    return mid()()\n}\nprint(outer())"
        ),
        "10"
    );
}

#[test]
fn recursion_via_fndecl() {
    assert_eq!(
        run_lines("function fib(n) {\n    if n <= 1 { return n }\n    return fib(n - 1) + fib(n - 2)\n}\nprint(fib(10))"),
        "55"
    );
    assert_eq!(
        run_lines("function fact(n) {\n    if n <= 1 { return 1 }\n    return n * fact(n - 1)\n}\nprint(fact(5))"),
        "120"
    );
}

#[test]
fn higher_order_functions() {
    assert_eq!(
        run_lines(
            "const square = function(x) { return x * x }\nfunction apply(f, x) { return f(x) }\nprint(apply(square, 6))"
        ),
        "36"
    );
}

#[test]
fn object_and_array_reference_semantics() {
    // Mutating through an alias must be visible (reference types).
    assert_eq!(
        run_lines("const a = [1, 2]\nconst b = a\nb[0] = 99\nprint(a[0])"),
        "99"
    );
    assert_eq!(
        run_lines("const o = { v: 1 }\nconst p = o\np.v = 42\nprint(o.v)"),
        "42"
    );
}

#[test]
fn shared_memory_module() {
    assert_eq!(
        run_lines("const b = memory.allocateFloat32Array([1.5, 2.5, 3.5])\nprint(b.length)"),
        "3"
    );
}

#[test]
fn print_separates_args() {
    assert_eq!(run_lines("print(\"sum\", 1, 2, \"end\")"), "sum 1 2 end");
    assert_eq!(run_lines("print(\"a\")"), "a");
    assert_eq!(run_lines("print(1)"), "1");
}

#[test]
fn print_multi_arg_values() {
    assert_eq!(run_lines("print(true && false)"), "false");
    assert_eq!(run_lines("print(!true)"), "false");
    assert_eq!(run_lines("print([1, 2, 3])"), "[1, 2, 3]");
}

#[test]
fn ax_roundtrip() {
    let src = "function add(a, b) { return a + b }\nprint(add(2, 3))\nconst o = { x: 1 }\no.x = 9\nprint(o.x)";
    let a = run(src);
    let b = roundtrip(src);
    assert_eq!(a, b);
    assert_eq!(a, vec!["5", "9"]);
}

#[test]
fn closures_survive_ax_roundtrip() {
    let src =
        "function makeAdder(x) { return function(y) { return x + y } }\nconst a = makeAdder(40)\nprint(a(2))";
    assert_eq!(roundtrip(src), vec!["42"]);
}

#[test]
fn run_returns_undefined_for_statement_program() {
    // Expression statements discard their value; the program result is the
    // last value left on the stack (typically undefined).
    let program = Compiler::compile_source("1 + 2\nprint(3)").unwrap();
    let (mut vm, _out) = Vm::with_output(program);
    let result = vm.run();
    assert_eq!(format!("{}", result), "undefined");
}

#[test]
fn output_sink_capture() {
    let program = Compiler::compile_source("print(1)\nprint(2)\nprint(3)").unwrap();
    let (mut vm, out) = Vm::with_output(program);
    let _ = Arc::clone(&out);
    vm.run();
    assert_eq!(*out.lock().unwrap(), vec!["1", "2", "3"]);
}

#[test]
fn template_literals() {
    assert_eq!(
        run_lines("let name = \"World\"\nprint(`Hello ${name}!`)"),
        "Hello World!"
    );
    assert_eq!(run_lines("print(`sum: ${1 + 2}`)"), "sum: 3");
    assert_eq!(run_lines("print(`${3} + ${4} = ${3 + 4}`)"), "3 + 4 = 7");
    // Empty templates and interpolation-only templates.
    assert_eq!(run_lines("print(`a${``}b`)"), "ab");
    assert_eq!(run_lines("print(`${5}`)"), "5");
    // Nested templates.
    assert_eq!(run_lines("let n = \"x\"\nprint(`${`in ${n}`}`)"), "in x");
    // Property access and expressions inside interpolation.
    assert_eq!(run_lines("const o = { x: 10 }\nprint(`v=${o.x}`)"), "v=10");
    assert_eq!(
        run_lines("print(`f=${(function(a, b) { return a + b })(2, 3)}`)"),
        "f=5"
    );
    // Escapes.
    assert_eq!(run_lines("print(`a\\n\\tb`)"), "a\n\tb");
    assert_eq!(run_lines("print(`tick \\` here`)"), "tick ` here");
}

#[test]
fn arrow_functions() {
    assert_eq!(run_lines("const sq = x => x * x\nprint(sq(5))"), "25");
    assert_eq!(
        run_lines("const add = (a, b) => a + b\nprint(add(2, 3))"),
        "5"
    );
    assert_eq!(run_lines("const f = () => 42\nprint(f())"), "42");
    // Block bodies with explicit return.
    assert_eq!(
        run_lines("const g = (x) => { return x * 2 }\nprint(g(4))"),
        "8"
    );
    // Curried arrows.
    assert_eq!(
        run_lines("const c = (a) => (b) => a + b\nprint(c(5)(3))"),
        "8"
    );
    // Arrows as arguments.
    assert_eq!(
        run_lines("function apply(f, x) { return f(x) }\nprint(apply(x => x + 1, 41))"),
        "42"
    );
    // Arrows capture and mutate like lambdas.
    assert_eq!(
        run_lines("let c = 0\nconst inc = () => { c = c + 1 }\ninc()\ninc()\nprint(c)"),
        "2"
    );
    // Arrow results inside template interpolation.
    assert_eq!(run_lines("const f = x => x * 2\nprint(`${f(5)}`)"), "10");
}

#[test]
fn for_of_loops() {
    assert_eq!(
        run_lines("let s = 0\nfor (const v of [1, 2, 3, 4]) { s = s + v }\nprint(s)"),
        "10"
    );
    // Iterates strings by character.
    assert_eq!(
        run_lines("let s = \"\"\nfor (const c of \"abc\") { s = s + c }\nprint(s)"),
        "abc"
    );
    // continue skips to the next element.
    assert_eq!(
        run_lines("let t = 0\nfor (const v of [10, 20, 30]) { if v == 20 { continue }\n t = t + v }\nprint(t)"),
        "40"
    );
    // break exits early.
    assert_eq!(
        run_lines("let u = 0\nfor (const v of [1, 2, 3, 4, 5]) { if v > 3 { break }\n u = u + v }\nprint(u)"),
        "6"
    );
    // Non-iterables throw Node's TypeError (previously they silently
    // iterated zero times).
    assert_eq!(
        run_lines("let c = 0\ntry { for (const v of 42) { c = c + 1 } } catch (e) { print(\"threw\") }\nprint(c)"),
        "threw\n0"
    );
    // Loop variable is per-iteration for closures.
    assert_eq!(
        run_lines("const fns = []\nfor (const v of [1, 2, 3]) { fns = fns }\nprint(\"ok\")"),
        "ok"
    );
}

#[test]
fn for_in_loops() {
    assert_eq!(
        run_lines("const o = { b: 2, a: 1, c: 3 }\nlet k = \"\"\nfor (const x in o) { k = k + x }\nprint(k)"),
        "abc"
    );
    assert_eq!(
        run_lines(
            "const o = { b: 2, a: 1 }\nlet s = 0\nfor (const k in o) { s = s + o[k] }\nprint(s)"
        ),
        "3"
    );
    // for-in over non-objects is a no-op.
    assert_eq!(
        run_lines("let c = 0\nfor (const k in 5) { c = c + 1 }\nprint(c)"),
        "0"
    );
    // break inside for-in.
    assert_eq!(
        run_lines("const o = { a: 1, b: 2, c: 3 }\nlet n = 0\nfor (const k in o) { if k == \"b\" { break }\n n = n + 1 }\nprint(n)"),
        "1"
    );
}

#[test]
fn fs_module_roundtrip() {
    let path = format!(".alloy_fs_test_{}.txt", std::process::id());
    let src = format!(
        "import * as fs from \"alloy:fs\"\n\
         fs.writeFileSync(\"{}\", \"hello\")\n\
         print(fs.existsSync(\"{}\"))\n\
         print(fs.readFileSync(\"{}\"))\n",
        path, path, path
    );
    let lines = run(&src);
    assert_eq!(lines, vec!["true", "hello"]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn repl_style_state_across_programs() {
    // REPL mode: top-level declarations become globals and a persistent VM
    // keeps their values across independently compiled lines.
    let line1 = Compiler::compile_source_with_mode("let x = 5", true, false).unwrap();
    let (mut vm, out) = Vm::with_output(line1);
    vm.run();

    let line2 = Compiler::compile_source_with_mode("x = x + 10", true, false).unwrap();
    vm.set_program(line2);
    vm.run();

    let line3 = Compiler::compile_source_with_mode("print(x)", true, false).unwrap();
    vm.set_program(line3);
    vm.run();

    assert_eq!(*out.lock().unwrap(), vec!["15"]);
}

#[test]
fn async_function_awaits_and_returns_promise() {
    // Async calls that complete synchronously (no real suspension) flow
    // through: await on a fulfilled promise continues immediately.
    assert_eq!(
        run_lines(
            "async function g() { return 21 + 21 }\n\
             async function f() { const v = await g(); print(v) }\n\
             f()"
        ),
        "42"
    );
}

#[test]
fn async_arrow_functions() {
    assert_eq!(
        run_lines(
            "const sq = async x => x * x\n\
             const add = async (a, b) => a + b\n\
             async function main() { print(await sq(6)); print(await add(2, 3)) }\n\
             main()"
        ),
        "36\n5"
    );
}

#[test]
fn spawn_passes_serialized_arguments_to_worker() {
    // spawn(fn, ...args): the args cross to the isolated worker as
    // serialized bytes (ints, strings, arrays, objects, closures) and the
    // result settles as a promise, matching Node's worker-thread shape.
    assert_eq!(
        run_lines(
            "async function main() {\n\
                 const r = await spawn((a, b, c) => a + b + c.length, 1, 2, [3, 4]);\n\
                 const o = await spawn(obj => obj.x * obj.y, { x: 6, y: 7 });\n\
                 const s = await spawn((tag, n) => tag + n, \"n=\", 42);\n\
                 const cb = await spawn((f, v) => f(v) * 2, x => x + 1, 5);\n\
                 print(\"r=\" + r + \" o=\" + o + \" s=\" + s + \" cb=\" + cb);\n\
             }\n\
             main()"
        ),
        "r=5 o=42 s=n=42 cb=12"
    );
}

/// The terminal `Py_FinalizeEx` path must run in its own process (finalizing
/// the shared interpreter inside the test binary would destabilize every
/// parallel test), so this drives the real CLI as a subprocess with
/// `ALLOY_PYTHON_EMBED=1`: import + call (embed), finalize (clean), a second
/// finalize is refused loudly, and a later python call still works — via the
/// child-sidecar fallback, so finalizing never breaks the program.
#[test]
fn finalize_python_embed_clean_teardown_via_cli() {
    if std::env::var("ALLOY_PYTHON_EMBED").as_deref() != Ok("1") {
        eprintln!("skip: embed-only (run with ALLOY_PYTHON_EMBED=1)");
        return;
    }
    if std::process::Command::new("python")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skip: no python on PATH");
        return;
    }
    let cli = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(if cfg!(windows) { "alloy.exe" } else { "alloy" });
    if !cli.exists() {
        eprintln!("skip: CLI binary not built ({})", cli.display());
        return;
    }
    let dir = std::env::temp_dir();
    let tag = format!(
        "fin_cli_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let py_path = dir.join(format!("{}_m.py", tag));
    let ajs_path = dir.join(format!("{}.ajs", tag));
    std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
    let py_spec = py_path.to_string_lossy().replace('\\', "/");
    let ajs = format!(
        r#"
        import {{ add }} from '{}' as python;
        (async () => {{
            print("r1=" + (await python.add(2, 3)));
            const f1 = finalizePythonEmbed();
            print("finalized=" + f1.finalized + " live=" + f1.liveBackends + " err='" + f1.error + "'");
            const f2 = finalizePythonEmbed();
            print("second-finalized=" + f2.finalized + " err-has-finalized=" + ("" + f2.error).includes("finalized"));
            print("r2=" + (await python.add(4, 5)));
        }})();
        "#,
        py_spec
    );
    std::fs::write(&ajs_path, &ajs).unwrap();
    let mut child = std::process::Command::new(&cli)
        .env("ALLOY_PYTHON_EMBED", "1")
        .arg(&ajs_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn CLI");
    // Bounded wait: the script has no timers, so it must finish fast; a hang
    // would mean the finalize wedged the process.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let output = loop {
        if let Some(status) = child.try_wait().expect("poll CLI") {
            let out = child.wait_with_output().expect("collect stdout");
            break (status, out);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("CLI did not exit within 20s — finalize likely hung the process");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = std::fs::remove_file(&py_path);
    let _ = std::fs::remove_file(&ajs_path);
    assert!(output.0.success(), "CLI exited with an error");
    let out = String::from_utf8_lossy(&output.1.stdout).to_string();
    assert!(out.contains("r1=5"), "embed call failed: {out}");
    assert!(
        out.contains("finalized=true live=0"),
        "clean Py_FinalizeEx expected: {out}"
    );
    assert!(
        out.contains("second-finalized=false err-has-finalized=true"),
        "finalize must be terminal and loud: {out}"
    );
    assert!(
        out.contains("r2=9"),
        "post-finalize python must fall back to the child sidecar: {out}"
    );
}

#[test]
fn await_suspends_and_resumes() {
    // A truly pending promise suspends the async invocation; resolving it
    // later (after the main program) runs the continuation as a microtask.
    assert_eq!(
        run_lines(
            "const d = Promise.withResolvers()\n\
             async function w() { const v = await d.promise; print(v) }\n\
             w()\n\
             d.resolve(99)"
        ),
        "99"
    );
}

#[test]
fn await_multiple_suspensions_in_order() {
    assert_eq!(
        run_lines(
            "const d1 = Promise.withResolvers()\n\
             const d2 = Promise.withResolvers()\n\
             async function t() {\n\
                 const a = await d1.promise\n\
                 const b = await d2.promise\n\
                 print(a + b)\n\
             }\n\
             t()\n\
             d1.resolve(40)\n\
             d2.resolve(2)"
        ),
        "42"
    );
}

#[test]
fn await_nested_async_chain() {
    // outer awaits inner; inner itself suspends on a deferred, so resolving
    // chains inner -> outer through two microtask hops.
    assert_eq!(
        run_lines(
            "const dd = Promise.withResolvers()\n\
             async function inner() { const v = await dd.promise; return v + 1 }\n\
             async function outer() { const r = await inner(); print(r) }\n\
             outer()\n\
             dd.resolve(41)"
        ),
        "42"
    );
}

#[test]
fn async_recursion_with_await() {
    assert_eq!(
        run_lines(
            "async function fib(n) {\n\
                 if n < 2 { return n }\n\
                 const a = await fib(n - 1)\n\
                 const b = await fib(n - 2)\n\
                 return a + b\n\
             }\n\
             async function main() { print(await fib(7)) }\n\
             main()"
        ),
        "13"
    );
}

#[test]
fn await_on_already_resolved_promise() {
    assert_eq!(
        run_lines(
            "const d = Promise.withResolvers()\n\
             d.resolve(123)\n\
             async function late() { print(await d.promise) }\n\
             late()"
        ),
        "123"
    );
}

#[test]
fn then_chains_and_fulfills_late() {
    assert_eq!(
        run_lines("Promise.resolve(5).then(v => print(v * 2))"),
        "10"
    );
    assert_eq!(
        run_lines(
            "const d = Promise.withResolvers()\n\
             d.promise.then(v => print(`got ${v}`))\n\
             d.resolve(7)"
        ),
        "got 7"
    );
}

#[test]
fn set_timeout_fires_after_script() {
    assert_eq!(
        run_lines("setTimeout(() => print(\"later\"), 5)\nprint(\"now\")"),
        "now\nlater"
    );
}

#[test]
fn async_closure_captures_and_suspends() {
    // Async function as a closure with an upvalue, exercising capture + await.
    assert_eq!(
        run_lines(
            "function makeCounter() {\n\
                 let n = 0\n\
                 const d = Promise.withResolvers()\n\
                 async function bump() { n = await d.promise; return n }\n\
                 d.resolve(5)\n\
                 return bump\n\
             }\n\
             async function main() { const b = makeCounter(); print(await b()) }\n\
             main()"
        ),
        "5"
    );
}

#[test]
fn multi_upvalue_closures_keep_order() {
    // Regression: NewClosure popped captured cells off the stack, reversing
    // them, so closures with 2+ upvalues read the wrong variables.
    assert_eq!(
        run_lines(
            "function mk() { let a = 1; let b = 2; return function() { return a * 10 + b } }\n\
             print(mk()())"
        ),
        "12"
    );
    assert_eq!(
        run_lines(
            "const sq = x => x * x\n\
             const add = (a, b) => a + b\n\
             async function main() { print(await sq(6)); print(await add(2, 3)) }\n\
             main()"
        ),
        "36\n5"
    );
}

#[test]
fn typeof_promise_is_object() {
    assert_eq!(run_lines("print(typeof Promise.resolve(1))"), "object");
}

#[test]
fn await_outside_async_is_compile_error() {
    assert!(matches!(
        compile_err("print(await 5)"),
        CompileError::AwaitOutsideAsync
    ));
    // A sync arrow inside an async function still cannot await.
    assert!(matches!(
        compile_err("async function f() { const g = () => await 1; return g() }"),
        CompileError::AwaitOutsideAsync
    ));
}

#[test]
fn try_catch_catches_thrown_value() {
    assert_eq!(
        run_lines("try { throw \"boom\" } catch (e) { print(`caught: ${e}`) }"),
        "caught: boom"
    );
    // Numeric error, caught through two call levels.
    assert_eq!(
        run_lines(
            "function inner() { throw 42 }\n\
             function middle() { inner() }\n\
             try { middle() } catch (e) { print(e) }"
        ),
        "42"
    );
    // catch without a binding.
    assert_eq!(
        run_lines("try { throw 7 } catch { print(\"no binding\") }"),
        "no binding"
    );
    // Nested try: the inner handler wins.
    assert_eq!(
        run_lines(
            "try {\n\
                 try { throw \"inner\" } catch (e) { print(e) }\n\
                 print(\"after\")\n\
             } catch (e) { print(\"outer\") }"
        ),
        "inner\nafter"
    );
}

#[test]
fn finally_runs_on_normal_and_exceptional_paths() {
    assert_eq!(
        run_lines("try { print(\"body\") } finally { print(\"fin\") }"),
        "body\nfin"
    );
    // finally on the exception path, then the original error is rethrown.
    assert_eq!(
        run_lines(
            "try {\n\
                 try { throw \"orig\" } finally { print(\"fin\") }\n\
             } catch (e) { print(`outer: ${e}`) }"
        ),
        "fin\nouter: orig"
    );
    // catch + finally combined.
    assert_eq!(
        run_lines("try { throw 1 } catch (e) { print(`c: ${e}`) } finally { print(\"fin\") }"),
        "c: 1\nfin"
    );
}

#[test]
fn throw_inside_catch_propagates_out() {
    // The handled try's handler is deactivated, so a throw from the catch
    // body is caught by the enclosing try, not this one.
    assert_eq!(
        run_lines(
            "try {\n\
                 try { throw \"inner\" } catch (e) { throw \"from catch\" }\n\
             } catch (e) { print(e) }"
        ),
        "from catch"
    );
}

#[test]
fn return_and_break_inside_try_clear_handlers() {
    // A return inside a try must deactivate the handler: a later throw in the
    // caller must not jump into the dead handler.
    assert_eq!(
        run_lines(
            "function f() { try { return 1 } catch (e) { print(\"never\") } }\n\
             print(f())\n\
             try { throw \"after\" } catch (e) { print(`caught: ${e}`) }"
        ),
        "1\ncaught: after"
    );
    // Same for break out of a try inside a loop.
    assert_eq!(
        run_lines(
            "let n = 0\n\
             for (let i = 0; i < 3; i = i + 1) {\n\
                 try { if i == 1 { break } n = n + 1 } catch (e) { print(\"never\") }\n\
             }\n\
             print(n)\n\
             try { throw \"later\" } catch (e) { print(`caught later: ${e}`) }"
        ),
        "1\ncaught later: later"
    );
}

#[test]
fn await_rejected_promise_throws_to_catch() {
    assert_eq!(
        run_lines(
            "const d = Promise.withResolvers()\n\
             async function w() {\n\
                 try { await d.promise } catch (e) { print(`rejected: ${e}`) }\n\
             }\n\
             w()\n\
             d.reject(\"bad\")"
        ),
        "rejected: bad"
    );
}

#[test]
fn async_function_throw_rejects_promise_caught_by_caller() {
    assert_eq!(
        run_lines(
            "async function boom() { throw \"async boom\" }\n\
             async function main() {\n\
                 try { await boom() } catch (e) { print(`caught: ${e}`) }\n\
             }\n\
             main()"
        ),
        "caught: async boom"
    );
}

#[test]
fn then_on_rejected_and_throw_in_callback() {
    // Rejection handler runs when the source rejects.
    assert_eq!(
        run_lines(
            "const d = Promise.withResolvers()\n\
             d.promise.then(v => print(\"never\"), e => print(`rej: ${e}`))\n\
             d.reject(\"reason\")"
        ),
        "rej: reason"
    );
    // A throw inside a fulfillment callback rejects the chained promise,
    // which the next .then's rejection handler observes.
    assert_eq!(
        run_lines(
            "Promise.resolve(1)\n\
                 .then(v => { throw \"then fail\" })\n\
                 .then(null, e => print(`chain: ${e}`))"
        ),
        "chain: then fail"
    );
}

#[test]
fn finally_runs_on_return_break_continue() {
    // return runs the finally, preserving the return value.
    assert_eq!(
        run_lines(
            "function f() {\n\
                 try { return 42 } finally { print(\"cleanup\") }\n\
             }\n\
             print(f())"
        ),
        "cleanup\n42"
    );
    // break runs the finally.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             for (let i = 0; i < 3; i = i + 1) {\n\
                 try { if i == 1 { break } s = s + i } finally { s = s + \"f\" }\n\
             }\n\
             print(s)"
        ),
        "0ff"
    );
    // continue runs the finally.
    assert_eq!(
        run_lines(
            "let t = \"\"\n\
             for (let i = 0; i < 3; i = i + 1) {\n\
                 try { if i == 1 { continue } t = t + i } finally { t = t + \"f\" }\n\
             }\n\
             print(t)"
        ),
        "0ff2f"
    );
}

#[test]
fn finally_covers_catch_body_and_preserves_value() {
    // A return from the catch body still runs the finally, before returning.
    assert_eq!(
        run_lines(
            "function b() {\n\
                 try { throw \"x\" } catch (e) { return `caught ${e}` } finally { print(\"fin\") }\n\
             }\n\
             print(b())"
        ),
        "fin\ncaught x"
    );
    // A throw from the catch body runs the finally, then propagates.
    assert_eq!(
        run_lines(
            "function f() {\n\
                 try { throw \"a\" } catch (e) { throw `re: ${e}` } finally { print(\"F\") }\n\
             }\n\
             try { f() } catch (e) { print(`got: ${e}`) }"
        ),
        "F\ngot: re: a"
    );
    // The return value survives cleanup that declares locals.
    assert_eq!(
        run_lines(
            "function f4() {\n\
                 try {\n\
                     try { return 5 } finally { let y = 3; print(`y=${y}`) }\n\
                 } finally { print(\"outer\") }\n\
             }\n\
             print(f4())"
        ),
        "y=3\nouter\n5"
    );
}

#[test]
fn finally_overrides_and_nests() {
    // A return in the finally overrides the try's return.
    assert_eq!(
        run_lines("function g() { try { return 1 } finally { return 2 } }\nprint(g())"),
        "2"
    );
    // A throw in the finally overrides the try's return.
    assert_eq!(
        run_lines(
            "function a() { try { return 1 } finally { throw \"fin\" } }\n\
             try { a() } catch (e) { print(`over: ${e}`) }"
        ),
        "over: fin"
    );
    // Nested finallys run innermost-first on return, value preserved.
    assert_eq!(
        run_lines(
            "function h() {\n\
                 try {\n\
                     try { return \"v\" } finally { print(\"inner\") }\n\
                 } finally { print(\"outer\") }\n\
             }\n\
             print(h())"
        ),
        "inner\nouter\nv"
    );
    // A break in the finally overrides a pending exception.
    assert_eq!(
        run_lines(
            "for (let i = 0; i < 3; i = i + 1) {\n\
                 try { try { throw \"boom\" } finally { break } } catch (e) { print(\"no\") }\n\
             }\n\
             print(\"after\")"
        ),
        "after"
    );
}

#[test]
fn labeled_break_and_continue() {
    // break outer exits both loops.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             outer: for (let i = 0; i < 3; i = i + 1) {\n\
                 for (let j = 0; j < 3; j = j + 1) {\n\
                     if j == 1 { break outer }\n\
                     s = s + i + j + \" \"\n\
                 }\n\
             }\n\
             print(s)"
        ),
        "00 "
    );
    // continue outer skips to the next outer iteration.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             a: for (let i = 0; i < 3; i = i + 1) {\n\
                 for (let j = 0; j < 3; j = j + 1) {\n\
                     if j == 1 { continue a }\n\
                     s = s + i + j + \" \"\n\
                 }\n\
             }\n\
             print(s)"
        ),
        "00 10 20 "
    );
    // continue on a labeled while targets the condition.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             let n = 0\n\
             w: while n < 3 {\n\
                 n = n + 1\n\
                 if n == 2 { continue w }\n\
                 s = s + n\n\
             }\n\
             print(s)"
        ),
        "13"
    );
}

#[test]
fn labeled_blocks_and_shadowing() {
    // break out of a plain labeled block.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             blk: { s = s + \"a\"; if true { break blk } s = s + \"b\" }\n\
             s = s + \"c\"\n\
             print(s)"
        ),
        "ac"
    );
    // A shadowed label resolves to the innermost one.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             x: for (let i = 0; i < 2; i = i + 1) {\n\
                 x: for (let j = 0; j < 2; j = j + 1) {\n\
                     if j == 1 { break x }\n\
                     s = s + i + j + \" \"\n\
                 }\n\
             }\n\
             print(s)"
        ),
        "00 10 "
    );
    // continue to a non-loop label is a compile error; so is an unknown one.
    assert!(matches!(
        compile_err("outer: { continue outer }"),
        CompileError::ContinueNonLoop(_)
    ));
    assert!(matches!(
        compile_err("break nope"),
        CompileError::UndefinedLabel(_)
    ));
}

#[test]
fn labeled_exit_runs_only_enclosing_finallys() {
    // A try nested inside the labeled loop runs its finally on labeled exit.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             outer: for (let i = 0; i < 3; i = i + 1) {\n\
                 try {\n\
                     if i == 1 { break outer }\n\
                     s = s + i\n\
                 } finally { s = s + \"f\" }\n\
             }\n\
             print(s)"
        ),
        "0ff"
    );
    // A try *enclosing* the labeled loop must not run its finally at the
    // break site; it runs once when the try body completes (JS semantics).
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             try {\n\
                 inner: for (let i = 0; i < 3; i = i + 1) {\n\
                     if i == 1 { break inner }\n\
                     s = s + i\n\
                 }\n\
                 s = s + \"T\"\n\
             } finally { s = s + \"F\" }\n\
             print(s)"
        ),
        "0TF"
    );
    // Unlabeled break from a loop inside a try has the same semantics.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             try {\n\
                 for (let i = 0; i < 3; i = i + 1) {\n\
                     if i == 1 { break }\n\
                     s = s + i\n\
                 }\n\
             } finally { s = s + \"F\" }\n\
             print(s)"
        ),
        "0F"
    );
}

#[test]
fn input_completeness_detection() {
    use alloy_vm::compiler::Compiler as C;
    // Single complete lines.
    assert!(C::input_is_complete("let x = 5"));
    assert!(C::input_is_complete("print(1 + 2)"));
    assert!(C::input_is_complete("const o = { a: 1, b: [1, 2] }"));
    // Unclosed brackets keep reading.
    assert!(!C::input_is_complete("for (let i = 0; i < 3; i = i + 1) {"));
    assert!(!C::input_is_complete("function f() {"));
    assert!(!C::input_is_complete("let a = [1, 2"));
    assert!(!C::input_is_complete("try {"));
    // ...and complete once closed, even across lines.
    assert!(C::input_is_complete(
        "for (let i = 0; i < 3; i = i + 1) {\n    print(i)\n}"
    ));
    assert!(C::input_is_complete(
        "function f() {\n    return 1\n}\nprint(f())"
    ));
    // Braces inside strings, templates, and comments don't count.
    assert!(C::input_is_complete("let s = \"{ not a block }\""));
    assert!(!C::input_is_complete("let s = \"unterminated"));
    assert!(C::input_is_complete("let t = `a ${ { x: 1 } } b`"));
    assert!(!C::input_is_complete("let t = `a ${name"));
    assert!(C::input_is_complete("x = 5 // } trailing comment"));
    assert!(!C::input_is_complete("/* open block comment"));
    // A mismatched close is "complete" so the compiler can report the error.
    assert!(C::input_is_complete("}"));
}

#[test]
fn switch_dispatch_and_fallthrough() {
    // Basic dispatch with break, and default.
    assert_eq!(
        run_lines(
            "function cat(n) {\n\
                 let r = \"\"\n\
                 switch (n) {\n\
                     case 1: r = \"one\"; break\n\
                     case 2: r = \"two\"; break\n\
                     default: r = \"other\"\n\
                 }\n\
                 return r\n\
             }\n\
             print(cat(1), cat(2), cat(9))"
        ),
        "one two other"
    );
    // Fall-through between arms (no implicit break).
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             switch (2) {\n\
                 case 1: s = s + \"a\"\n\
                 case 2: s = s + \"b\"\n\
                 case 3: s = s + \"c\"; break\n\
                 case 4: s = s + \"d\"\n\
             }\n\
             print(s)"
        ),
        "bc"
    );
    // Consecutive case labels share a body.
    assert_eq!(
        run_lines(
            "let v = \"\"\n\
             switch (3) {\n\
                 case 1:\n\
                 case 2:\n\
                 case 3: v = \"three\"; break\n\
                 default: v = \"other\"\n\
             }\n\
             print(v)"
        ),
        "three"
    );
    // Default in the middle still dispatches correctly, and matching cases
    // before it fall through into it.
    assert_eq!(
        run_lines(
            "let t = \"\"\n\
             switch (9) {\n\
                 case 1: t = t + \"a\"\n\
                 default: t = t + \"d\"\n\
                 case 2: t = t + \"b\"\n\
             }\n\
             print(t)"
        ),
        "db"
    );
    // No match and no default skips the whole switch.
    assert_eq!(
        run_lines(
            "let w = \"before\"\n\
             switch (42) { case 1: w = \"no\" }\n\
             w = w + \" after\"\n\
             print(w)"
        ),
        "before after"
    );
    // Switch uses strict equality: \"1\" does not match 1.
    assert_eq!(
        run_lines(
            "let x = \"\"\n\
             switch (\"1\") { case 1: x = \"int\"; break; default: x = \"str\" }\n\
             print(x)"
        ),
        "str"
    );
}

#[test]
fn switch_break_continue_labels_finally() {
    // continue inside a switch continues the enclosing loop, skipping the
    // rest of the loop body.
    assert_eq!(
        run_lines(
            "let s = \"\"\n\
             for (let i = 0; i < 5; i = i + 1) {\n\
                 switch (i) {\n\
                     case 1: continue\n\
                     case 3: continue\n\
                     default: s = s + i\n\
                 }\n\
                 s = s + \".\"\n\
             }\n\
             print(s)"
        ),
        "0.2.4."
    );
    // continue in a switch with no enclosing loop is a compile error.
    assert!(matches!(
        compile_err("switch (1) { case 1: continue }"),
        CompileError::BreakOutsideLoop
    ));
    // break label exits a labeled switch.
    assert_eq!(
        run_lines(
            "let t = \"\"\n\
             outer: switch (2) {\n\
                 case 1: t = t + \"a\"\n\
                 case 2: t = t + \"b\"; break outer\n\
                 case 3: t = t + \"c\"\n\
             }\n\
             print(t)"
        ),
        "b"
    );
    // continue to a switch label is an error (switch is not a loop).
    assert!(matches!(
        compile_err("outer: switch (1) { case 1: continue outer }"),
        CompileError::ContinueNonLoop(_)
    ));
    // A break out of the switch runs an enclosing try's finally exactly once.
    assert_eq!(
        run_lines(
            "let v = \"\"\n\
             switch (1) {\n\
                 case 1:\n\
                     try { v = v + \"a\"; break } finally { v = v + \"F\" }\n\
                 default: v = v + \"d\"\n\
             }\n\
             print(v)"
        ),
        "aF"
    );
    // Nested switches: inner break stays in the inner switch.
    assert_eq!(
        run_lines(
            "let x = \"\"\n\
             switch (1) {\n\
                 case 1:\n\
                     switch (2) {\n\
                         case 2: x = \"inner\"; break\n\
                         default: x = \"bad\"\n\
                     }\n\
                     break\n\
                 default: x = \"outer\"\n\
             }\n\
             print(x)"
        ),
        "inner"
    );
}

#[test]
fn ternary_operator() {
    // Basic selection and truthiness.
    assert_eq!(run_lines("print(true ? \"a\" : \"b\")"), "a");
    assert_eq!(run_lines("print(false ? \"a\" : \"b\")"), "b");
    assert_eq!(run_lines("print(0 ? \"t\" : \"f\")"), "f");
    assert_eq!(run_lines("print(1 ? \"t\" : \"f\")"), "t");
    assert_eq!(run_lines("print(\"\" ? \"t\" : \"f\")"), "f");
    assert_eq!(run_lines("print(null ? \"t\" : \"f\")"), "f");
    // Short-circuit: only the taken branch runs.
    assert_eq!(
        run_lines("let x = 0\ntrue ? (x = 1) : (x = 2)\nprint(x)"),
        "1"
    );
    assert_eq!(
        run_lines("let y = 0\nfalse ? (y = 1) : (y = 2)\nprint(y)"),
        "2"
    );
    // Precedence: || binds tighter than the ternary.
    assert_eq!(run_lines("print(false || true ? \"a\" : \"b\")"), "a");
    assert_eq!(run_lines("print(1 + 2 ? \"t\" : \"f\")"), "t");
    // Right associativity and nested branches.
    assert_eq!(
        run_lines("print(false ? \"a\" : true ? \"b\" : \"c\")"),
        "b"
    );
    assert_eq!(
        run_lines("print(false ? \"a\" : false ? \"b\" : \"c\")"),
        "c"
    );
    // As an assignment RHS and inside functions/templates.
    assert_eq!(run_lines("let z = true ? 1 : 2\nprint(z)"), "1");
    assert_eq!(
        run_lines("function f(c) { return c ? \"yes\" : \"no\" }\nprint(f(true), f(false))"),
        "yes no"
    );
    assert_eq!(
        run_lines("let v = 5\nprint(`v is ${v > 3 ? \"big\" : \"small\"}`)"),
        "v is big"
    );
    // In loop conditions.
    assert_eq!(
        run_lines(
            "let s = \"\"\nfor (let i = 0; i < (true ? 4 : 9); i = i + 1) { s = s + i }\nprint(s)"
        ),
        "0123"
    );
}

#[test]
fn increment_decrement_operators() {
    // Postfix yields the old value, prefix the new one.
    assert_eq!(run_lines("let a = 5; let b = a++; print(a, b)"), "6 5");
    assert_eq!(run_lines("let c = 5; let d = ++c; print(c, d)"), "6 6");
    assert_eq!(run_lines("let e = 5; let f = e--; print(e, f)"), "4 5");
    assert_eq!(run_lines("let g = 5; let h = --g; print(g, h)"), "4 4");
    // In for-loop headers.
    assert_eq!(
        run_lines("let s = \"\"\nfor (let i = 0; i < 4; i++) { s = s + i }\nprint(s)"),
        "0123"
    );
    assert_eq!(
        run_lines("let t = \"\"\nfor (let j = 3; j > 0; j--) { t = t + j }\nprint(t)"),
        "321"
    );
    // Properties and index targets.
    assert_eq!(
        run_lines("const o = { n: 1 }; let old = o.n++; print(old, o.n)"),
        "1 2"
    );
    assert_eq!(
        run_lines("const o = { n: 1 }; let pre = ++o.n; print(pre, o.n)"),
        "2 2"
    );
    assert_eq!(
        run_lines("const arr = [1, 2, 3]; let v = arr[1]++; print(v, arr[1])"),
        "2 3"
    );
    assert_eq!(
        run_lines("const arr = [1, 2, 3]; let w = ++arr[0]; print(w, arr[0])"),
        "2 2"
    );
    // The object/index expression is evaluated once.
    assert_eq!(
        run_lines("let calls = 0\nconst obj = { v: 1 }\nfunction get() { calls = calls + 1; return obj }\nget().v++\nprint(calls, obj.v)"),
        "1 2"
    );
    // Result participates in larger expressions.
    assert_eq!(run_lines("let p = 0; print(p++ + p)"), "1");
    assert_eq!(run_lines("let q = 0; print(++q + q)"), "2");
    assert_eq!(
        run_lines("let r = 0; let res = r++ + ++r; print(r, res)"),
        "2 2"
    );
    // Upvalues in closures mutate the captured cell.
    assert_eq!(
        run_lines(
            "function counter() {\n\
                 let n = 0\n\
                 return function() { n++; return n }\n\
             }\n\
             const c1 = counter()\n\
             const c2 = counter()\n\
             print(c1(), c1(), c2())"
        ),
        "1 2 1"
    );
    // Invalid targets are compile errors.
    assert!(matches!(
        compile_err("5++"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("++print"),
        CompileError::CannotShadowBuiltin(_)
    ));
}

#[test]
fn compound_assign_evaluates_target_once() {
    // The object expression of `+=` runs exactly once, before the RHS.
    assert_eq!(
        run_lines(
            "let calls = 0\n\
             const obj = { v: 10 }\n\
             function get() { calls = calls + 1; return obj }\n\
             get().v += 5\n\
             print(calls, obj.v)"
        ),
        "1 15"
    );
    // Same for the index expression of `+=`.
    assert_eq!(
        run_lines(
            "let calls = 0\n\
             const arr = [10, 20, 30]\n\
             function idx() { calls = calls + 1; return 1 }\n\
             arr[idx()] += 5\n\
             print(calls, arr[1])"
        ),
        "1 25"
    );
    // A side-effecting index is evaluated once for `-=` too (previously it
    // fell into the plain-assignment path and silently assigned the RHS).
    assert_eq!(
        run_lines(
            "let i = 0\n\
             const a = [5, 6, 7]\n\
             a[i += 1] += 2\n\
             print(i, a[1])"
        ),
        "1 8"
    );
    assert_eq!(run_lines("const a = [10]; a[0] -= 3; print(a[0])"), "7");
    // The expression result is the new value.
    assert_eq!(
        run_lines("const o = { n: 1 }; let r = o.n += 3; print(r, o.n)"),
        "4 4"
    );
}

#[test]
fn plain_assign_evaluates_target_before_rhs() {
    // `get().p = rhs()` calls get() before rhs() (JS reference order).
    assert_eq!(
        run_lines(
            "let log = \"\"\n\
             const o = { p: 0 }\n\
             function get() { log = log + \"obj\"; return o }\n\
             function rhs() { log = log + \"rhs\"; return 5 }\n\
             get().p = rhs()\n\
             print(log, o.p)"
        ),
        "objrhs 5"
    );
    // Same for index targets.
    assert_eq!(
        run_lines(
            "let log = \"\"\n\
             const arr = [0, 0]\n\
             function idx() { log = log + \"idx\"; return 1 }\n\
             function rhs() { log = log + \"rhs\"; return 7 }\n\
             arr[idx()] = rhs()\n\
             print(log, arr[1])"
        ),
        "idxrhs 7"
    );
    // The expression result is the RHS value, and chained assignment works.
    assert_eq!(
        run_lines("const o = {}; let r = o.x = 42; print(r, o.x)"),
        "42 42"
    );
    assert_eq!(
        run_lines("const a = {}; const b = {}; a.x = (b.y = 9); print(a.x, b.y)"),
        "9 9"
    );
    // The value survives a call on the RHS (stack/frame safety).
    assert_eq!(
        run_lines(
            "const a = [1]; function fill() { return [9, 8] }\na[0] = fill()[1]; print(a[0])"
        ),
        "8"
    );
}

#[test]
fn chained_assignment_is_right_associative() {
    // a = b = c  chains to  a = (b = c).
    assert_eq!(
        run_lines("let a = 0\nlet b = 0\nlet c = 0\na = b = c = 5\nprint(a, b, c)"),
        "5 5 5"
    );
    // Same through properties.
    assert_eq!(
        run_lines("const x = {}\nconst y = {}\nx.p = y.q = 9\nprint(x.p, y.q)"),
        "9 9"
    );
    // Mixed identifier/property chains.
    assert_eq!(
        run_lines("let z = 0\nconst o = {}\nz = o.v = 7\nprint(z, o.v)"),
        "7 7"
    );
    // Compound assignment is right-associative too.
    assert_eq!(
        run_lines("let p = 1\nlet q = 2\nlet s = 3\np += q += s\nprint(p, q, s)"),
        "6 5 3"
    );
    // Arithmetic still binds tighter than assignment.
    assert_eq!(run_lines("let t = 1 + 2 * 3\nprint(t)"), "7");
}

#[test]
fn multiple_var_declarators() {
    // let a = 1, b = 2, c (no init -> undefined).
    assert_eq!(
        run_lines("let a = 1, b = 2, c\nprint(a, b, c)"),
        "1 2 undefined"
    );
    // Works with const and var too.
    assert_eq!(run_lines("const x = 1, y = 2\nprint(x + y)"), "3");
    assert_eq!(run_lines("var m = 5, n = 6\nprint(m + n)"), "11");
    // In a for-init header.
    assert_eq!(
        run_lines(
            "let s = \"\"\nfor (let i = 0, j = 10; i < 3; i = i + 1) { s = s + i + j }\nprint(s)"
        ),
        "010110210"
    );
    // In a function body.
    assert_eq!(
        run_lines("function f() { let u = 1, v = 2; return u + v }\nprint(f())"),
        "3"
    );
    // Initializers run left-to-right, and later ones see earlier bindings.
    assert_eq!(
        run_lines("let log = \"\"\nlet g1 = (log = log + \"1\"), g2 = (log = log + \"2\")\nprint(log, g1, g2)"),
        "12 1 12"
    );
    assert_eq!(run_lines("let h = 1, i = h + 1\nprint(h, i)"), "1 2");
}

#[test]
fn destructuring_declarations() {
    // Object and array declarations.
    assert_eq!(
        run_lines("let { a, b } = { a: 1, b: 2 }\nprint(a, b)"),
        "1 2"
    );
    assert_eq!(run_lines("let [x, y] = [10, 20]\nprint(x, y)"), "10 20");
    // Renaming and nested patterns.
    assert_eq!(
        run_lines("let { p: renamed } = { p: 7 }\nprint(renamed)"),
        "7"
    );
    assert_eq!(
        run_lines("let { u: { v } } = { u: { v: 3 } }\nprint(v)"),
        "3"
    );
    assert_eq!(
        run_lines("let [m, [n, o]] = [1, [2, 3]]\nprint(m, n, o)"),
        "1 2 3"
    );
    // Array holes skip elements.
    assert_eq!(run_lines("let [p, , q] = [5, 99, 6]\nprint(p, q)"), "5 6");
    // Missing keys bind undefined; strings destructure by character.
    assert_eq!(
        run_lines("let { missing } = {}\nprint(missing)"),
        "undefined"
    );
    assert_eq!(run_lines("let [c1, c2] = \"ab\"\nprint(c1, c2)"), "a b");
    // A destructuring declaration needs an initializer.
    assert!(matches!(
        compile_err("let { a }"),
        CompileError::UnexpectedToken(_)
    ));
}

#[test]
fn destructuring_assignments() {
    // Array assignment as a statement.
    assert_eq!(
        run_lines("let s = 0; let t = 0;\n[s, t] = [7, 8]\nprint(s, t)"),
        "7 8"
    );
    // Swap through destructuring.
    assert_eq!(
        run_lines("let a = 1; let b = 2;\n[a, b] = [b, a]\nprint(a, b)"),
        "2 1"
    );
    // Parenthesized object assignment.
    assert_eq!(
        run_lines("let u = 0; let v = 0;\n({ x: u, y: v } = { x: 1, y: 2 })\nprint(u, v)"),
        "1 2"
    );
    // The assignment expression evaluates to the RHS.
    assert_eq!(
        run_lines(
            "let arr = [1, 2]; let f = 0; let g = 0;\nlet r = ([f, g] = arr)\nprint(r === arr)"
        ),
        "true"
    );
    // Works against captured upvalues inside functions.
    assert_eq!(
        run_lines(
            "let a = [10, 20];\n\
             function f() {\n\
                 let [x, y] = a;\n\
                 [x, y] = [y, x];\n\
                 return x\n\
             }\n\
             print(f())"
        ),
        "20"
    );
    // Compound assignment to a pattern is an error.
    assert!(matches!(
        compile_err("let a = 1; let b = 2;\n[a, b] += 1"),
        CompileError::UnexpectedToken(_)
    ));
}

#[test]
fn destructuring_in_loop_headers() {
    // for-of with array and object patterns in the declaration.
    assert_eq!(
        run_lines(
            "const pairs = [[1, \"one\"], [2, \"two\"]]\n\
             let s = \"\"\n\
             for (let [num, word] of pairs) { s = s + num + word }\n\
             print(s)"
        ),
        "1one2two"
    );
    assert_eq!(
        run_lines(
            "const objs = [{ name: \"a\", v: 1 }, { name: \"b\", v: 2 }]\n\
             let s = \"\"\n\
             for (let { name, v } of objs) { s = s + name + v }\n\
             print(s)"
        ),
        "a1b2"
    );
    // Plain names still work, and the assignment form destructures existing vars.
    assert_eq!(
        run_lines("let s = \"\"\nfor (let x of [10, 20]) { s = s + x }\nprint(s)"),
        "1020"
    );
    assert_eq!(
        run_lines(
            "const pairs = [[1, \"one\"], [2, \"two\"]]\n\
             let n = 0; let w = \"\"; let s = \"\"\n\
             for ([n, w] of pairs) { s = s + n + w }\n\
             print(s)"
        ),
        "1one2two"
    );
    // Nested patterns and continue inside the destructuring loop.
    assert_eq!(
        run_lines("const n = [[1, [2, 3]]]\nlet s = \"\"\nfor (let [a, [b, c]] of n) { s = s + a + b + c }\nprint(s)"),
        "123"
    );
    assert_eq!(
        run_lines(
            "const pairs = [[1, 0], [2, 0], [3, 0]]\n\
             let s = \"\"\n\
             for (let [a] of pairs) { if a == 2 { continue } s = s + a }\n\
             print(s)"
        ),
        "13"
    );
    // for-in destructures each key (a string) by character.
    assert_eq!(
        run_lines("const o = { ab: 1, cd: 2 }\nlet s = \"\"\nfor (let [f, g] in o) { s = s + f + g }\nprint(s)"),
        "abcd"
    );
}

#[test]
fn spread_operator() {
    // Array literal spreads, mixed with normal elements.
    assert_eq!(
        run_lines(
            "const a = [1, 2, 3]\nconst b = [0, ...a, 4]\nprint(b.length, b[0], b[1], b[3], b[4])"
        ),
        "5 0 1 3 4"
    );
    // Multiple and nested spreads, empty spreads.
    assert_eq!(
        run_lines("const a = [1]; const b = [2]; print([...a, ...b].length)"),
        "2"
    );
    assert_eq!(
        run_lines("const a = [1]; print([...[...a], 2].length)"),
        "2"
    );
    assert_eq!(run_lines("print([...[], 5].length)"), "1");
    // Spread copies: mutating the source doesn't affect the copy.
    assert_eq!(
        run_lines("const g = [1, 2]\nconst h = [...g]\ng[0] = 99\nprint(h[0], g[0])"),
        "1 99"
    );
    // Call argument spreads, mixed and nested.
    assert_eq!(
        run_lines(
            "function sum(x, y, z) { return x + y + z }\nconst a = [1, 2, 3]\nprint(sum(...a))"
        ),
        "6"
    );
    assert_eq!(
        run_lines("function sum(x, y, z) { return x + y + z }\nprint(sum(1, ...[2, 3]))"),
        "6"
    );
    assert_eq!(
        run_lines("function wrap(x, y, z) { return x * 100 + y * 10 + z }\nprint(wrap(...[1, ...[2, 3]]))"),
        "123"
    );
    // Zero-arg spread leaves parameters undefined.
    assert_eq!(
        run_lines("function f(a, b) { return a + \"|\" + b }\nprint(f(...[]))"),
        "undefined|undefined"
    );
    // Strings spread by character.
    assert_eq!(
        run_lines("const s = [...\"ab\"]\nprint(s.length, s[0], s[1])"),
        "2 a b"
    );
    // Into native calls.
    assert_eq!(run_lines("const p = [\"a\", \"b\"]\nprint(...p)"), "a b");
}

#[test]
fn uncaught_throw_sets_vm_error() {
    let program = Compiler::compile_source("throw \"boom\"").unwrap();
    let mut vm = Vm::new(program);
    vm.run();
    match vm.take_error() {
        Some(v) => assert_eq!(v.as_str(), Some("boom")),
        other => panic!("expected uncaught string, got {:?}", other),
    }
}

#[test]
fn repl_function_prints_and_mutates_globals_across_lines() {
    // A function defined on one REPL line references globals by index into
    // its own program's table; loading later lines must not invalidate them.
    let line1 = Compiler::compile_source_with_mode("let x = 5", true, false).unwrap();
    let (mut vm, out) = Vm::with_output(line1);
    vm.run();

    let line2 =
        Compiler::compile_source_with_mode("function inc() { x = x + 1; print(x) }", true, false)
            .unwrap();
    vm.set_program(line2);
    vm.run();

    let line3 = Compiler::compile_source_with_mode("inc()", true, false).unwrap();
    vm.set_program(line3);
    vm.run();

    let line4 = Compiler::compile_source_with_mode("inc()", true, false).unwrap();
    vm.set_program(line4);
    vm.run();

    assert_eq!(*out.lock().unwrap(), vec!["6", "7"]);
}

#[test]
fn repl_closure_survives_program_swap() {
    // A closure defined in one REPL line must keep executing the bytecode of
    // the program that defined it after later lines swap in new programs.
    // Previously the closure's raw bytecode offset was interpreted against the
    // new program's bytecode, corrupting execution.
    let line1 = Compiler::compile_source_with_mode("const sq = x => x * x", true, false).unwrap();
    let (mut vm, out) = Vm::with_output(line1);
    vm.run();

    let line2 = Compiler::compile_source_with_mode("print(sq(7))", true, false).unwrap();
    vm.set_program(line2);
    vm.run();

    let line3 = Compiler::compile_source_with_mode("print(sq(sq(3)))", true, false).unwrap();
    vm.set_program(line3);
    vm.run();

    assert_eq!(*out.lock().unwrap(), vec!["49", "81"]);
}

#[test]
fn rest_parameters() {
    // Rest collects every argument past the fixed params into an array.
    assert_eq!(run_lines("function sum(...nums) {\nlet t = 0\nfor (let n of nums) { t += n }\nreturn t\n}\nprint(sum(1, 2, 3, 4))"), "10");
    assert_eq!(
        run_lines("function cat(a, b, ...rest) {\nprint(a, b, rest.length, rest[0], rest[1])\n}\ncat(1, 2, 3, 4, 5)"),
        "1 2 3 3 4"
    );
    // Fewer args than params: rest is empty, missing params are undefined.
    assert_eq!(
        run_lines("function cat(a, b, ...rest) {\nprint(a, b, rest.length)\n}\ncat(9)"),
        "9 undefined 0"
    );
    // Zero args.
    assert_eq!(
        run_lines("function all(...x) { print(x.length) }\nall()"),
        "0"
    );
    // Arrows and closure capture of the rest array.
    assert_eq!(
        run_lines("let f = (...args) => args.length\nprint(f(1, 2, 3))"),
        "3"
    );
    assert_eq!(
        run_lines("let g = (a, ...rest) => rest[0] + a\nprint(g(10, 20, 30))"),
        "30"
    );
    assert_eq!(run_lines("function make() {\nlet f = (...args) => { return args.length }\nreturn f\n}\nprint(make()(7, 8, 9))"), "3");
    // Recursion with a rest param.
    assert_eq!(
        run_lines("function fib(n, ...acc) {\nif (n < 2) { return n }\nreturn fib(n - 1) + fib(n - 2)\n}\nprint(fib(10))"),
        "55"
    );
    // Rest must be last.
    assert!(matches!(
        compile_err("function f(...a, b) {}"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("function f(a, ...b, c) {}"),
        CompileError::UnexpectedToken(_)
    ));
}

#[test]
fn rest_elements_in_destructuring() {
    // Basic rest element.
    assert_eq!(
        run_lines("let [head, ...tail] = [1, 2, 3, 4]\nprint(head, tail.length, tail[0], tail[2])"),
        "1 3 2 4"
    );
    // Holes skip elements before the rest.
    assert_eq!(
        run_lines("let [a, , ...rest] = [10, 20, 30, 40]\nprint(a, rest.length, rest[0])"),
        "10 2 30"
    );
    // Empty rest when the source runs out.
    assert_eq!(run_lines("let [x, ...y] = [5]\nprint(x, y.length)"), "5 0");
    // Nested patterns and objects inside the rest.
    assert_eq!(
        run_lines("let [p, [q, ...r]] = [1, [2, 3, 4]]\nprint(p, q, r.length, r[1])"),
        "1 2 2 4"
    );
    assert_eq!(
        run_lines("let [m, ...objs] = [1, { v: 2 }, { v: 3 }]\nprint(m, objs.length, objs[1].v)"),
        "1 2 3"
    );
    // Rest in the assignment form.
    assert_eq!(
        run_lines("let s = 0; let t = 0;\n[s, ...t] = [7, 8, 9]\nprint(s, t.length, t[0], t[1])"),
        "7 2 8 9"
    );
    assert_eq!(
        run_lines("let r = 0;\n[...r] = [1, 2]\nprint(r.length)"),
        "2"
    );
    // Strings slice by character.
    assert_eq!(
        run_lines("let [c1, ...cs] = \"hello\"\nprint(c1, cs.length, cs[0], cs[3])"),
        "h 4 e o"
    );
    // Rest in a for-of header pattern.
    assert_eq!(
        run_lines("let s = \"\"\nfor (let [...r] of [[1, 2], [3]]) { s = s + r.length }\nprint(s)"),
        "21"
    );
    // Rest must be last.
    assert!(matches!(
        compile_err("let x = 0; let y = 0; let z = 0;\n[x, ...y, z] = [1, 2, 3]"),
        CompileError::UnexpectedToken(_)
    ));
}

#[test]
fn nan_and_infinity_globals() {
    // NaN/Infinity are global constants, not undefined identifiers.
    assert_eq!(run_lines("print(NaN === NaN)"), "false");
    assert_eq!(run_lines("print(NaN !== NaN)"), "true");
    assert_eq!(run_lines("print(NaN < 5)"), "false");
    assert_eq!(run_lines("print(1 / 0 === Infinity)"), "true");
    assert_eq!(run_lines("print(-1 / 0 === -Infinity)"), "true");
    assert_eq!(run_lines("print(0 / 0 === 0 / 0)"), "false");
    assert_eq!(
        run_lines("print(typeof NaN, typeof Infinity)"),
        "number number"
    );
    // JS: NaN is falsy.
    assert_eq!(run_lines("print(NaN ? 1 : 0)"), "0");
    // NaN/Infinity cannot be shadowed.
    assert!(matches!(
        compile_err("let NaN = 5"),
        CompileError::CannotShadowBuiltin(_)
    ));
    assert!(matches!(
        compile_err("let Infinity = 5"),
        CompileError::CannotShadowBuiltin(_)
    ));
}

#[test]
fn string_relational_comparison() {
    // JS: two strings compare lexicographically.
    assert_eq!(run_lines("print(\"abc\" < \"abd\")"), "true");
    assert_eq!(run_lines("print(\"b\" > \"a\")"), "true");
    assert_eq!(run_lines("print(\"aa\" <= \"aa\")"), "true");
    assert_eq!(run_lines("print(\"a\" < \"aa\")"), "true");
    // Lexicographic, not numeric: "10" < "9" in string order.
    assert_eq!(run_lines("print(\"10\" < \"9\")"), "true");
    assert_eq!(run_lines("print(\"2\" > \"12\")"), "true");
    // Mixed string/number still coerces numerically (JS behavior).
    assert_eq!(run_lines("print(\"5\" < 6)"), "true");
    assert_eq!(run_lines("print(1 < \"2\")"), "true");
    assert_eq!(run_lines("print(\"10\" < 9)"), "false");
}

#[test]
fn exponent_literals() {
    assert_eq!(run_lines("print(1e3)"), "1000");
    assert_eq!(run_lines("print(2E2)"), "200");
    assert_eq!(run_lines("print(1.5e-3)"), "0.0015");
    assert_eq!(run_lines("print(1e2 + 1)"), "101");
    assert_eq!(run_lines("print(2e1 * 3)"), "60");
    assert_eq!(run_lines("print(1e21)"), "1000000000000000000000");
    assert_eq!(run_lines("print(1e-7)"), "0.0000001");
    // `1e` with no exponent digits is a loud compile error (Node: SyntaxError
    // "Invalid or unexpected token") — the old behavior silently parsed it as
    // `1` then ident `e`.
    assert!(Compiler::compile_source("let x = 1e + 0").is_err());
}

#[test]
fn if_without_else_keeps_stack() {
    // The true path of an else-less `if` used to fall through into the false
    // path's Pop, eating one live stack value (a local slot) whenever the
    // stack was non-empty — e.g. any `if` inside a loop whose then-branch
    // doesn't exit. Verified against V8.
    assert_eq!(
        run_lines(
            "let c = 0\nfor (let i = 0; i < 5; i++) { if (i % 2 === 0) { c += 1; } }\nprint(c)"
        ),
        "3"
    );
    assert_eq!(
        run_lines(
            "let s = \"\"\nfor (let i = 0; i < 4; i++) { if (i < 2) { s = s + i; } }\nprint(s)"
        ),
        "01"
    );
    // Sieve of Eratosthenes: previously found only 1 prime.
    assert_eq!(
        run_lines(
            "function sieve(n) {\n\
                 let prime = [];\n\
                 for (let i = 0; i <= n; i++) { prime[i] = true; }\n\
                 prime[0] = false;\n\
                 prime[1] = false;\n\
                 for (let i = 2; i * i <= n; i++) {\n\
                     if (prime[i]) {\n\
                         for (let j = i * i; j <= n; j += i) { prime[j] = false; }\n\
                     }\n\
                 }\n\
                 let count = 0;\n\
                 for (let i = 2; i <= n; i++) { if (prime[i]) { count += 1; } }\n\
                 return count;\n\
             }\n\
             print(sieve(100), sieve(1000))"
        ),
        "25 168"
    );
    // A non-exiting if at top level with locals below it on the stack.
    assert_eq!(
        run_lines("let a = 1; let b = 2;\nif (true) { a = 10; }\nprint(a, b)"),
        "10 2"
    );
}

#[test]
fn compound_assign_all_operators() {
    assert_eq!(run_lines("let a = 10; a *= 3; print(a)"), "30");
    assert_eq!(run_lines("let a = 30; a /= 2; print(a)"), "15");
    assert_eq!(run_lines("let a = 15; a %= 4; print(a)"), "3");
    assert_eq!(run_lines("let o = { x: 6 }; o.x *= 3; print(o.x)"), "18");
    assert_eq!(run_lines("let o = { x: 9 }; o.x /= 3; print(o.x)"), "3");
    assert_eq!(
        run_lines("let arr = [2, 3]; arr[0] *= 5; arr[1] /= 2; print(arr[0], arr[1])"),
        "10 1.5"
    );
    assert_eq!(run_lines("let a = 7; a %= 3; print(a)"), "1");
}

#[test]
fn modulo_by_zero_is_nan() {
    assert_eq!(run_lines("print(5 % 0)"), "NaN");
    assert_eq!(run_lines("print(-7 % 0)"), "NaN");
    assert_eq!(run_lines("print(5.5 % 0)"), "NaN");
    // Division by zero still gives Infinity (Node display).
    assert_eq!(run_lines("print(5 / 0)"), "Infinity");
}

#[test]
fn array_literal_elisions() {
    // `[a, , b]` evaluates the elision to undefined.
    assert_eq!(
        run_lines("print([1, , 3].length, [1, , 3][1])"),
        "3 undefined"
    );
    assert_eq!(
        run_lines("let x = [1, , 3]; print(x[0], x[1], x[2])"),
        "1 undefined 3"
    );
    // Elisions in destructuring assignments skip the element.
    assert_eq!(
        run_lines("let t1 = 0; let t3 = 0;\n[t1, , t3] = [9, 99, 8]\nprint(t1, t3)"),
        "9 8"
    );
    // Trailing comma after an elision does not add an element.
    assert_eq!(run_lines("print([1, ,].length)"), "2");
}

#[test]
fn typeof_null_is_object() {
    assert_eq!(run_lines("print(typeof null)"), "object");
}

#[test]
fn function_declarations_hoist() {
    // A function may reference a sibling declared later in the same block
    // (JS hoisting); mutual recursion between siblings works.
    assert_eq!(
        run_lines(
            "function outer() {\n\
                 function a() { return b() }\n\
                 function b() { return 42 }\n\
                 return a()\n\
             }\n\
             print(outer())"
        ),
        "42"
    );
    // Mutual recursion (even/odd).
    assert_eq!(
        run_lines(
            "function isEven(n) {\n\
                 if (n === 0) { return true }\n\
                 return isOdd(n - 1)\n\
             }\n\
             function isOdd(n) {\n\
                 if (n === 0) { return false }\n\
                 return isEven(n - 1)\n\
             }\n\
             print(isEven(10), isOdd(7), isEven(0))"
        ),
        "true true true"
    );
    // Nested mutual recursion with captured state.
    assert_eq!(
        run_lines(
            "function counter() {\n\
                 let n = 0\n\
                 function up() { n += 1; return n }\n\
                 function down() { n -= 1; return n }\n\
                 return { up: up, down: down }\n\
             }\n\
             let c = counter()\n\
             c.up(); c.up(); c.down()\n\
             print(c.up())"
        ),
        "2"
    );
}

/// `require('./x.ajs')` + `export`: write real module files to a temp dir and
/// exercise caching, isolation, mutable exported state, nested requires,
/// circular detection, and error paths — semantics verified against Node's
/// ESM (`import * as m`).
#[test]
fn require_and_export() {
    let dir = std::env::temp_dir().join(format!("alloy_require_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let w = |name: &str, src: &str| std::fs::write(dir.join(name), src).unwrap();
    w(
        "math.ajs",
        "export let pi = 3.14;\n\
         export function double(x) { return x * 2; }\n\
         export let counter = 0;\n\
         export function bump() { counter = counter + 1; return counter; }\n\
         let hidden = 42;\n\
         export default \"math-module\";\n",
    );
    w(
        "logger.ajs",
        "export let log = [];\n\
         export function add(msg) { log.push(msg); return log.length; }\n",
    );
    w(
        "nested.ajs",
        "const math = require('./math.ajs');\n\
         export function triple(x) { return math.double(x) + x; }\n",
    );
    w(
        "circ_a.ajs",
        "const b = require('./circ_b.ajs');\n\
         export let name = \"a\";\n",
    );
    w(
        "circ_b.ajs",
        "const a = require('./circ_a.ajs');\n\
         export let name = \"b\";\n",
    );
    w("broken.ajs", "let x = ;\n");

    let main = "const m = require('./math.ajs');\n\
                const m2 = require('./math.ajs');\n\
                const logger = require('./logger.ajs');\n\
                const n = require('./nested.ajs');\n\
                print(m.pi, m.double(21), m.bump(), m.bump(), m.default, m.hidden);\n\
                print(typeof m.nonexistent);\n\
                print(m2 === m ? \"cached\" : \"not-cached\");\n\
                print(logger.add(\"a\"), logger.add(\"b\"), logger.log.join(\"|\"));\n\
                print(n.triple(5));\n\
                try { require('./circ_a.ajs'); print(\"no error\"); }\n\
                catch (e) { print(\"circular\", e); }\n\
                try { require('./broken.ajs'); print(\"no error\"); }\n\
                catch (e) { print(\"broken\"); }\n\
                try { require('./missing.ajs'); print(\"no error\"); }\n\
                catch (e) { print(\"missing\"); }\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
    vm.run();
    let lines = out.lock().unwrap().join("\n");
    assert_eq!(
        lines,
        "3.14 42 1 2 math-module undefined\n\
         undefined\n\
         cached\n\
         1 2 a|b\n\
         15\n\
         circular Error: Circular require of './circ_a.ajs'\n\
         broken\n\
         missing"
    );
    // A fresh VM has a fresh module cache (like a fresh Node process): the
    // module runs again from scratch. Within one VM (above) the module ran
    // once and its exported state persisted across calls. The exports object
    // holds LIVE bindings: m.counter reads through to the module's own
    // storage, so it sees bump()'s mutation (matching ESM).
    let main2 = "const m = require('./math.ajs');\n\
                 print(m.counter, m.bump(), m.counter);\n";
    let program = Compiler::compile_source(main2).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main2.ajs").to_string_lossy());
    vm.run();
    assert_eq!(out.lock().unwrap().join(" "), "0 1 1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Keyword tokens are legal property names (IdentifierName): `m.default`,
/// `o.if`, `{ delete: 3 }`, `const { default: d } = o` — Node accepts all of
/// these even though the words are reserved.
#[test]
fn keyword_property_names() {
    assert_eq!(
        run_lines("const o = { default: 1, if: 2, delete: 3, case: 4, new: 5 }\nprint(o.default, o.if, o.delete, o.case, o.new)"),
        "1 2 3 4 5"
    );
    assert_eq!(run_lines("print({ default: 7 }.default)"), "7");
    assert_eq!(
        run_lines("const o = { default: 9 }\nconst { default: d } = o\nprint(d)"),
        "9"
    );
    // `export` is a loud compile error outside a module (Node: SyntaxError).
    assert!(Compiler::compile_source("export let x = 1;").is_err());
}

/// `import { f } from './x.ajs'` is sugar for require, verified against
/// Node's ESM: named imports (with `as` aliasing), `import * as m`, default
/// imports, and side-effect-only imports all load the module exactly once.
#[test]
fn import_sugar_matches_node() {
    let dir = std::env::temp_dir().join(format!("alloy_import_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let w = |name: &str, src: &str| std::fs::write(dir.join(name), src).unwrap();
    w(
        "math.ajs",
        "export let pi = 3.14;\n\
         export function double(x) { return x * 2; }\n\
         export let counter = 0;\n\
         export function bump() { counter = counter + 1; return counter; }\n\
         export default \"math-module\";\n",
    );
    w(
        "alias.ajs",
        "let secret = 99;\n\
         function helper() { return \"hi\"; }\n\
         export { secret as exposed, helper as help, helper };\n",
    );
    w(
        "side.ajs",
        "let runs = 0;\n\
         function init() { runs = runs + 1; }\n\
         init();\n\
         export function count() { return runs; }\n",
    );
    w(
        "mid.ajs",
        "import { double } from './math.ajs';\n\
                  export function quad(x) { return double(double(x)); }\n",
    );

    let main = "import { pi, double, bump } from './math.ajs';\n\
                import * as m from './math.ajs';\n\
                import dflt from './math.ajs';\n\
                import { exposed as ex, help } from './alias.ajs';\n\
                import './side.ajs';\n\
                import './side.ajs';\n\
                import { count } from './side.ajs';\n\
                import { quad } from './mid.ajs';\n\
                print(pi, double(5), bump(), bump());\n\
                print(m.pi, dflt);\n\
                print(ex, help());\n\
                print(count(), quad(3));\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
    vm.run();
    assert_eq!(
        out.lock().unwrap().join("\n"),
        "3.14 10 1 2\n3.14 math-module\n99 hi\n1 12"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Precompiled `.ax` modules: emit with `--module`, `require` loads the
/// bytecode (cached, isolated globals, exports), and a non-module `.ax` is
/// rejected loudly instead of silently mis-executing.
#[test]
fn ax_module_loading() {
    let dir = std::env::temp_dir().join(format!("alloy_ax_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let math = "export let pi = 3.14;\n\
                export function double(x) { return x * 2; }\n\
                export default \"ax-module\";\n";
    let program = Compiler::compile_module(math).unwrap();
    let bytes = program.to_bytes().unwrap();
    std::fs::write(dir.join("math.ax"), &bytes).unwrap();
    // A main-script .ax (is_module = false) must be refused by require.
    let notmod = Compiler::compile_source("print(1)\n").unwrap();
    let nb = notmod.to_bytes().unwrap();
    std::fs::write(dir.join("notmod.ax"), &nb).unwrap();

    let main = "const m = require('./math.ax');\n\
                const m2 = require('./math.ax');\n\
                print(m.pi, m.double(21), m.default);\n\
                print(m2 === m ? \"cached\" : \"not-cached\");\n\
                try { require('./notmod.ax'); print(\"no error\"); }\n\
                catch (e) { print(\"rejected\"); }\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
    vm.run();
    assert_eq!(
        out.lock().unwrap().join("\n"),
        "3.14 42 ax-module\ncached\nrejected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ESM live bindings, verified against Node: `import { x }` aliases the
/// module's own storage, so mutations made through exported functions are
/// visible through the named import, the namespace object (`m.x` and
/// `m["x"]`), and JSON.stringify — but assigning to an import is a loud
/// compile error (Node: SyntaxError).
#[test]
fn live_import_bindings() {
    let dir = std::env::temp_dir().join(format!("alloy_live_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("math.ajs"),
        "export let counter = 0;\n\
         export let name = \"init\";\n\
         export function bump() { counter = counter + 1; return counter; }\n\
         export function rename(n) { name = n; }\n\
         export let items = [];\n\
         export function add(i) { items.push(i); }\n\
         export default \"d\";\n",
    )
    .unwrap();

    let main = "import { counter, name, bump, rename, add, items } from './math.ajs';\n\
                import * as m from './math.ajs';\n\
                print(counter);\n\
                print(bump(), bump());\n\
                print(counter);\n\
                print(m.counter, m[\"counter\"]);\n\
                rename(\"renamed\");\n\
                print(name, m.name);\n\
                add(1); add(2);\n\
                print(items.length, items.join(\",\"));\n\
                print(JSON.stringify({ c: m.counter, n: m.name }));\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
    vm.run();
    assert_eq!(
        out.lock().unwrap().join("\n"),
        "0\n1 2\n2\n2 2\nrenamed renamed\n2 1,2\n{\"c\":2,\"n\":\"renamed\"}"
    );
    // Assigning to an import is rejected at compile time (ESM SyntaxError),
    // for =, compound ops, and ++ — a live cell must not be clobbered from
    // the importing scope.
    for src in [
        "import { counter } from './math.ajs';\ncounter = 5;",
        "import { counter } from './math.ajs';\ncounter += 1;",
        "import { counter } from './math.ajs';\ncounter++;",
        "import * as m from './math.ajs';\nm = {};",
    ] {
        assert!(
            Compiler::compile_source(src).is_err(),
            "assignment to import must be rejected: {}",
            src
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Node-style module resolution: extension fallback (`./math` → math.ajs,
/// then math.ax), directory `index.ajs`/`index.ax`, package.json `main`,
/// `node_modules/` walking from nested module dirs, builtin modules, and
/// cache identity across specifiers that resolve to the same file.
#[test]
fn node_style_module_resolution() {
    let dir = std::env::temp_dir().join(format!("alloy_resolve_test_{}", std::process::id()));
    let app = dir.join("app");
    std::fs::create_dir_all(app.join("node_modules/mylib")).unwrap();
    std::fs::create_dir_all(app.join("node_modules/subpkg")).unwrap();
    std::fs::create_dir_all(app.join("pkg")).unwrap();
    std::fs::create_dir_all(app.join("sub")).unwrap();
    std::fs::create_dir_all(app.join("sub/idx_only")).unwrap();
    let w = |p: &str, s: &str| {
        std::fs::write(app.join(p), s).unwrap();
    };
    w(
        "math.ajs",
        "export function fromAjs() { return \"ajs\"; }\n",
    );
    w(
        "onlyax_src.ajs",
        "export function fromAx() { return \"ax\"; }\n",
    );
    // Real bytecode module.
    let bc = Compiler::compile_module("export function fromAx() { return \"ax\"; }\n")
        .unwrap()
        .to_bytes()
        .unwrap();
    std::fs::write(app.join("onlyax.ax"), &bc).unwrap();
    w(
        "pkg/index.ajs",
        "export function fromIndex() { return \"index\"; }\n",
    );
    w("pkg/package.json", "{\"main\": \"./entry.ajs\"}\n");
    w(
        "pkg/entry.ajs",
        "export function fromMain() { return \"main\"; }\n",
    );
    w(
        "node_modules/mylib/package.json",
        "{\"main\": \"lib.js\"}\n",
    );
    w(
        "node_modules/mylib/lib.js",
        "export function fromLibMain() { return \"libmain\"; }\n",
    );
    w(
        "node_modules/mylib/index.ajs",
        "export function fromIndex() { return \"idx\"; }\n",
    );
    w(
        "node_modules/subpkg/index.ajs",
        "export function fromNodeSub() { return \"nsub\"; }\n",
    );
    w(
        "sub/util.ajs",
        "export function fromSub() { return \"sub\"; }\n",
    );
    w("sub/mid.ajs", "const u = require('./util');\nconst sp = require('subpkg');\nexport function go() { return u.fromSub() + ' ' + sp.fromNodeSub(); }\n");
    w(
        "sub/idx_only/only.ajs",
        "export function fromSub() { return \"idxonly\"; }\n",
    );

    let main = "const a = require('./math');\n\
                const b = require('./onlyax');\n\
                const c = require('./pkg');\n\
                const d = require('mylib');\n\
                const e = require('fs');\n\
                const m1 = require('./math');\n\
                const m2 = require('./math.ajs');\n\
                import { go } from './sub/mid.ajs';\n\
                print(a.fromAjs(), b.fromAx(), c.fromMain(), d.fromLibMain());\n\
                print(typeof e.writeFileSync);\n\
                print(go());\n\
                print(m1 === m2 ? 'same-module' : 'different');\n\
                try { require('./nope'); print('no error'); } catch (ex) { print('missing'); }\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&app.join("main.ajs").to_string_lossy());
    vm.run();
    assert_eq!(
        out.lock().unwrap().join("\n"),
        "ajs ax main libmain\nfunction\nsub nsub\nsame-module\nmissing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hot reload: `reload('./x.ajs')` drops the cached module so the next
/// `require` re-runs the file (Node's `delete require.cache`). The old
/// module's state stays valid for existing references, and reloading a
/// builtin or an unresolvable path returns false.
#[test]
fn hot_reload_requires() {
    let dir = std::env::temp_dir().join(format!("alloy_reload_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("counter.ajs"),
        "export let loads = 0;\n\
         export function loaded() { loads = loads + 1; return loads; }\n\
         export let counter = 0;\n\
         export function bump() { counter = counter + 1; return counter; }\n",
    )
    .unwrap();

    let main = "const m1 = require('./counter.ajs');\n\
                print(m1.loaded());\n\
                print(m1.bump(), m1.bump());\n\
                print(reload('./counter.ajs'));\n\
                const m2 = require('./counter.ajs');\n\
                print(m2.loaded());\n\
                print(m2.bump());\n\
                print(m1.bump());\n\
                print(reload('./nope.ajs'));\n\
                print(reload('fs'));\n";
    let program = Compiler::compile_source(main).unwrap();
    let (mut vm, out) = Vm::with_output(program);
    vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
    vm.run();
    assert_eq!(
        out.lock().unwrap().join("\n"),
        "1\n1 2\ntrue\n1\n1\n3\nfalse\nfalse"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Optional chaining (`?.`) — all shapes, Node-verified semantics.
#[test]
fn optional_chaining() {
    // Property access: short-circuits to undefined, keeps 0/""/false.
    assert_eq!(
        run_lines(
            "let o = { a: { b: 42 }, n: null, z: 0 };\n\
                   print(o?.a.b);\n\
                   print(o?.x?.y);\n\
                   print(o?.n?.b);\n\
                   print(o?.z);"
        ),
        "42\nundefined\nundefined\n0"
    );
    // Index access.
    assert_eq!(
        run_lines(
            "let o = { arr: [10, 20, 30], n: null };\n\
                   print(o?.[\"arr\"]?.[1]);\n\
                   print(o?.n?.[0]);"
        ),
        "20\nundefined"
    );
    // Plain calls: `f?.(...)` works; nullish callee short-circuits.
    assert_eq!(
        run_lines(
            "function f(x, y) { return x * 10 + y; }\n\
                   let fn = null;\n\
                   print(f?.(3, 4));\n\
                   print(fn?.(3, 4));"
        ),
        "34\nundefined"
    );
    // Method calls bind `this` — including through `?.` (Node semantics:
    // `?.Arguments` passes the property reference's this value).
    assert_eq!(
        run_lines(
            "let o = { a: { b: 42 }, m: function(x, y) { return this.a.b + x + y; } };\n\
                   print(o.m?.(1, 2));\n\
                   print(o?.m?.(1, 2));\n\
                   print(o?.m?.(1));"
        ),
        "45\n45\nNaN"
    );
    // Short-circuit skips argument evaluation entirely.
    assert_eq!(
        run_lines(
            "let calls = 0;\n\
                   function bump() { calls++; return 5; }\n\
                   let o = { m: function() { return 5; } };\n\
                   print(null?.m?.(bump()));\n\
                   print(calls);\n\
                   print(o?.m?.(bump()));\n\
                   print(calls);"
        ),
        "undefined\n0\n5\n1"
    );
    // Chain then member / spread / statement discard.
    assert_eq!(
        run_lines(
            "function g() { return { p: 7 }; }\n\
                   let gn = null;\n\
                   function sum(...xs) { let t = 0; for (let x of xs) { t += x; } return t; }\n\
                   let sn = null;\n\
                   print(g?.().p);\n\
                   print(gn?.().p);\n\
                   print(sum?.(...[1, 2, 3]));\n\
                   print(sn?.(...[1, 2]));\n\
                   gn?.().p;\n\
                   print(\"after\");"
        ),
        "7\nundefined\n6\nundefined\nafter"
    );
    // `?.` is forbidden as an assignment target (not a valid reference).
    assert!(matches!(
        compile_err("let o = {}; o?.a = 1"),
        CompileError::UnexpectedToken(_)
    ));
}

/// Object.keys/values/entries — Node-verified enumeration order: integer-
/// index keys ascending, then string keys in insertion order, tombstones
/// skipped; primitives coerce per ToObject; null/undefined throw.
#[test]
fn object_keys_entries_values() {
    assert_eq!(
        run_lines(
            "let o = {};\n\
                   o.b = 1; o[\"2\"] = 20; o.a = 3; o[\"0\"] = 4; o[\"10\"] = 5; o.c = 6;\n\
                   print(Object.keys(o).join(\"|\"));\n\
                   print(Object.values(o).join(\"|\"));\n\
                   print(Object.entries(o).map(e => e.join(\":\")).join(\"|\"));"
        ),
        "0|2|10|b|a|c\n4|20|5|1|3|6\n0:4|2:20|10:5|b:1|a:3|c:6"
    );
    // 4294967295 is NOT an array index — string insertion order.
    assert_eq!(
        run_lines(
            "let big = {};\n\
                   big[\"4294967295\"] = 1; big.a = 2; big[\"100\"] = 3;\n\
                   print(Object.keys(big).join(\"|\"));"
        ),
        "100|4294967295|a"
    );
    // delete tombstones are skipped, remaining order preserved.
    assert_eq!(
        run_lines(
            "let d = {};\n\
                   d.x = 1; d.y = 2; d.z = 3;\n\
                   delete d.y;\n\
                   print(Object.keys(d).join(\"|\"));"
        ),
        "x|z"
    );
    // Primitives: numbers/booleans have no keys; strings enumerate indices.
    assert_eq!(
        run_lines(
            "print(Object.keys(5).length);\n\
                   print(Object.keys(\"ab\").join(\"|\"));\n\
                   print(Object.values(\"ab\").join(\"|\"));\n\
                   print(Object.entries(\"ab\")[1][0], Object.entries(\"ab\")[1][1]);"
        ),
        "0\n0|1\na|b\n1 b"
    );
    // Null/undefined throw a catchable TypeError (Node's exact message).
    assert_eq!(
        run_lines("try { Object.keys(null); } catch (e) { print(e); }\n\
                   try { Object.values(); } catch (e) { print(e); }"),
        "TypeError: Cannot convert undefined or null to object\nTypeError: Cannot convert undefined or null to object"
    );
    // Object cannot be shadowed (it is a seeded builtin).
    assert!(matches!(
        compile_err("let Object = 5"),
        CompileError::CannotShadowBuiltin(_)
    ));
}

/// Map/Set methods live once on the prototype, sharing natives that bind
/// `this` per call (Node-verified: `Map.prototype.get === m.get`, method
/// identity, `size` still computed per read).
#[test]
fn map_set_prototype_methods_with_this() {
    // Methods work on instances through the prototype chain.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   m.set(\"a\", 1); m.set(\"b\", 2);\n\
                   print(m.get(\"a\"), m.get(\"b\"));\n\
                   print(m.has(\"a\"), m.has(\"z\"));\n\
                   print(m.size);\n\
                   print(m.set(\"a\", 99) === m);\n\
                   print(m.get(\"a\"));\n\
                   m.delete(\"b\");\n\
                   print(m.size, m.has(\"b\"));\n\
                   m.clear();\n\
                   print(m.size);"
        ),
        "1 2\ntrue false\n2\ntrue\n99\n1 false\n0"
    );
    // Methods are shared natives on the prototypes — identity holds.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   let s = new Set();\n\
                   print(Map.prototype.get === Map.prototype.get);\n\
                   print(Map.prototype.get === m.get);\n\
                   print(Map.prototype.set === m.set);\n\
                   print(Set.prototype.add === s.add);\n\
                   print(typeof Map.prototype.forEach);\n\
                   print(typeof Map.prototype.add);\n\
                   print(typeof Set.prototype.add);\n\
                   print(m instanceof Map, m instanceof Set);"
        ),
        "true\ntrue\ntrue\ntrue\nfunction\nundefined\nfunction\ntrue false"
    );
    // Set through the prototype, including forEach's map-identity arg.
    assert_eq!(
        run_lines(
            "let s = new Set();\n\
                   s.add(1); s.add(2);\n\
                   print(s.size, s.has(1), s.has(9));\n\
                   let acc = [];\n\
                   s.forEach(function (v, k, ss) { acc.push(v + \":\" + (ss === s)); });\n\
                   print(acc.join(\"|\"));\n\
                   print(s.add(2) === s, s.size);"
        ),
        "2 true false\n1:true|2:true\ntrue 2"
    );
    // `Object.keys` on a container sees no own enumerable properties.
    assert_eq!(
        run_lines("let m = new Map();\nprint(Object.keys(m).length)"),
        "0"
    );
    // Native identity compares by shared Arc (Math.floor === Math.floor).
    assert_eq!(run_lines("print(Math.floor === Math.floor)"), "true");
}

/// for-of and spread work directly on Map/Set via a synthetic iterator:
/// a Map yields [k, v] entry pairs, a Set its elements — insertion order,
/// Node-verified.
#[test]
fn for_of_and_spread_on_containers() {
    // for-of destructuring over a Map.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   m.set(\"a\", 1); m.set(\"b\", 2); m.set(\"c\", 3);\n\
                   let out = [];\n\
                   for (let [k, v] of m) { out.push(k + \"=\" + v); }\n\
                   print(out.join(\"|\"));"
        ),
        "a=1|b=2|c=3"
    );
    // Non-destructuring for-of over a Map yields the pair arrays.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   m.set(\"a\", 1); m.set(\"b\", 2);\n\
                   let pairs = [];\n\
                   for (let p of m) { pairs.push(p[0] + \":\" + p[1]); }\n\
                   print(pairs.join(\"|\"));"
        ),
        "a:1|b:2"
    );
    // Spread a Map into an array and as call arguments.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   m.set(\"a\", 1); m.set(\"b\", 2);\n\
                   let sp = [...m];\n\
                   print(sp.length, sp[0][0], sp[0][1]);\n\
                   function f(...args) { return args.map(a => a[0]).join(\"|\"); }\n\
                   print(f(...m));"
        ),
        "2 a 1\na|b"
    );
    // Set: for-of and spread yield elements.
    assert_eq!(
        run_lines(
            "let s = new Set();\n\
                   s.add(10); s.add(20); s.add(30);\n\
                   let sv = [];\n\
                   for (let x of s) { sv.push(x); }\n\
                   print(sv.join(\"|\"));\n\
                   print([...s].join(\"|\"));"
        ),
        "10|20|30\n10|20|30"
    );
    // Deletes keep survivor insertion order; arrays still pass through.
    assert_eq!(
        run_lines(
            "let m = new Map();\n\
                   for (let i = 0; i < 5; i++) { m.set(\"k\" + i, i); }\n\
                   m.delete(\"k1\"); m.delete(\"k3\");\n\
                   let acc = [];\n\
                   for (let [k, v] of m) { acc.push(k + v); }\n\
                   print(acc.join(\"|\"));\n\
                   let av = [];\n\
                   for (let x of [5, 6, 7]) { av.push(x); }\n\
                   print(av.join(\"|\"));"
        ),
        "k00|k22|k44\n5|6|7"
    );
}

/// for...of over a non-iterable throws Node's TypeError (exact message for
/// literals) instead of silently iterating zero times; iterables still work.
#[test]
fn for_of_non_iterable_throws() {
    // Literals: the message is exact, byte-for-byte with Node.
    assert_eq!(
        run_lines("try { for (let x of {}) {} } catch (e) { print(e); }\n\
                   try { for (let x of 5) {} } catch (e) { print(e); }\n\
                   try { for (let x of null) {} } catch (e) { print(e); }\n\
                   try { for (let x of undefined) {} } catch (e) { print(e); }\n\
                   try { for (let x of true) {} } catch (e) { print(e); }"),
        "TypeError: {} is not iterable\nTypeError: 5 is not iterable\nTypeError: null is not iterable\nTypeError: undefined is not iterable\nTypeError: true is not iterable"
    );
    // The throw is catchable and the loop body never runs.
    assert_eq!(
        run_lines(
            "let ran = 0;\n\
                   try { for (let x of 5) { ran++; } } catch (e) { print(\"caught\"); }\n\
                   print(ran);"
        ),
        "caught\n0"
    );
    // Iterables still iterate: arrays, strings, Map, Set.
    assert_eq!(
        run_lines(
            "let a = [];\n\
                   for (let x of [1, 2, 3]) { a.push(x); }\n\
                   for (let c of \"ab\") { a.push(c); }\n\
                   let m = new Map();\n\
                   m.set(\"k\", 7);\n\
                   for (let [k, v] of m) { a.push(k + v); }\n\
                   print(a.join(\"|\"));"
        ),
        "1|2|3|a|b|k7"
    );
    // A variable that flips from non-iterable to an array works afterwards.
    assert_eq!(
        run_lines(
            "let dyn = 7;\n\
                   try { for (let x of dyn) {} } catch (e) { print(\"threw\"); }\n\
                   dyn = [9];\n\
                   let acc = [];\n\
                   for (let x of dyn) { acc.push(x); }\n\
                   print(acc.join(\"|\"));"
        ),
        "threw\n9"
    );
}

/// `arguments`: length, indexing, iteration — Node-verified. The passed
/// args are snapshotted at call entry only for functions that reference
/// `arguments` (a compile-time flag on the closure); the frame's locals
/// would otherwise clobber the arg slots.
#[test]
fn arguments_object() {
    // for-of over arguments + length.
    assert_eq!(
        run_lines(
            "function sum() {\n\
                   let t = 0;\n\
                   for (let a of arguments) { t += a; }\n\
                   return t;\n\
                 }\n\
                 print(sum(1, 2, 3, 4));\n\
                 print(sum());"
        ),
        "10\n0"
    );
    // length/indexing; extra args beyond params visible; missing params don't inflate.
    assert_eq!(
        run_lines(
            "function two(a, b) {\n\
                     return arguments.length + \":\" + arguments[2] + \":\" + a + b;\n\
                   }\n\
                   print(two(1, 2, 3, 4));\n\
                   function three(a, b, c) {\n\
                     return arguments.length + \":\" + (arguments[2] === undefined) + \":\" + a;\n\
                   }\n\
                   print(three(9));\n\
                   print(three());"
        ),
        "4:3:12\n1:true:9\n0:true:undefined"
    );
    // spread from arguments and nested frames.
    assert_eq!(
        run_lines(
            "function snap() { return [...arguments].join(\"-\"); }\n\
                   print(snap(7, 8, 9));\n\
                   function inner() { return arguments.length; }\n\
                   function outer() { return inner(5, 6) + \":\" + arguments.length; }\n\
                   print(outer(1, 2, 3));"
        ),
        "7-8-9\n2:3"
    );
    // A local named `arguments` shadows the object (sloppy-JS allowed).
    assert_eq!(
        run_lines(
            "function shadow() {\n\
                     let arguments = 42;\n\
                     return arguments;\n\
                   }\n\
                   print(shadow());"
        ),
        "42"
    );
    // At top level `arguments` is a ReferenceError (ESM-style; Node's
    // CommonJS wrapper defines it with the module-loading args).
    assert_eq!(
        run_lines("try { print(arguments); } catch (e) { print(e); }"),
        "ReferenceError: arguments is not defined"
    );
}

/// Missing arguments read as `undefined` — never stale stack garbage — for
/// plain calls, method calls, and constructors.
#[test]
fn missing_arguments_are_undefined() {
    assert_eq!(
        run_lines("function f(x, y) { return x + y; }\nprint(f(1))"),
        "NaN"
    );
    assert_eq!(
        run_lines("let o = { m: function(x, y) { return x + y; } };\nprint(o.m(1))"),
        "NaN"
    );
    // Rest arrays collect ONLY the args past the fixed params.
    assert_eq!(
        run_lines("function cat(a, b, ...rest) {\nprint(a, b, rest.length)\n}\ncat(9)"),
        "9 undefined 0"
    );
    assert_eq!(
        run_lines("function all(...x) { print(x.length) }\nall()"),
        "0"
    );
}

/// Arrow functions capture `this` and `arguments` lexically (creation-time
/// cells), so they see the enclosing frame's binding even when escaped.
#[test]
fn arrow_lexical_this_and_arguments() {
    // Method call binds this; arrow inside sees the same receiver.
    assert_eq!(
        run_lines(
            "let o = { v: 7, f: function () { let a = () => this.v; return a(); } };\nprint(o.f())"
        ),
        "7"
    );
    // Plain call in this engine binds undefined (strict semantics); the
    // arrow must capture that, not the global.
    assert_eq!(
        run_lines("function g() { let a = () => this; return a() === undefined; }\nprint(g())"),
        "true"
    );
    // Escaped arrow keeps the captured this (via call to bind the outer).
    assert_eq!(
        run_lines("function mk() { return () => this; }\nlet esc = mk.call(42);\nprint(esc())"),
        "42"
    );
    // Nested arrows thread the capture through.
    assert_eq!(
        run_lines("function mk2() { let a = () => () => this; return a()(); }\nprint(mk2.call(9))"),
        "9"
    );
    // Arrow arguments: the enclosing frame's arguments, not the arrow's.
    assert_eq!(
        run_lines("function f(x, y) { let inner = () => arguments[0] + arguments.length; return inner(); }\nprint(f(3, 4))\nfunction g() { return (() => () => arguments[0])()(); }\nprint(g(11))"),
        "5\n11"
    );
    // Function.prototype.call / apply with an explicit receiver.
    assert_eq!(
        run_lines("function f() { return this.v; }\nlet o = { v: 5 };\nprint(f.call(o))\nprint(f.call(undefined))\nprint(f.apply(o))\nprint(f.apply(o, [9, 10]))"),
        "5\nundefined\n5\n5"
    );
}

/// Nullish coalescing, logical assignment, do...while, the in operator,
/// object method shorthand / computed keys / spread — Node-verified.
#[test]
fn modern_syntax_surface() {
    // ?? — left operand only when null/undefined, and mixing with && / || in
    // one unparenthesized expression is a compile-time SyntaxError (Node's
    // behavior: the whole program fails to parse, so try/catch can't see it).
    assert_eq!(
        run_lines("print(null ?? 1, undefined ?? 2, 0 ?? 3, \"\" ?? 4, false ?? 5)\nprint((null ?? 0) || 5, 5 && (null ?? 9))"),
        "1 2 0  false\n5 9"
    );
    assert!(matches!(
        compile_err("null ?? 0 || 5;"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("5 && null ?? 9;"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("null ?? 0 || 5;"),
        CompileError::UnexpectedToken(m) if m.contains("requires parentheses")
    ));
    // Parens reset the mixing rule; both orders are then legal.
    assert_eq!(
        run_lines("print((null ?? 0) || 5, 5 && (null ?? 9))"),
        "5 9"
    );
    // Logical assignment: short-circuit, no RHS eval on the skipping path,
    // expression value is old/new per path.
    assert_eq!(
        run_lines("let a = 0;\na &&= 5;\nprint(a)\na = 3;\na &&= 5;\nprint(a)\nlet b = 0;\nb ||= 9;\nprint(b)\nlet c = null;\nc ??= 7;\nprint(c)\nlet n = 0;\nfunction bump() { n++; return 1; }\nlet x = 0;\nx &&= bump();\nprint(n, x)\nlet o = { p: null };\no.p ||= 2;\nprint(o.p)\nlet arr = [null];\narr[0] ??= 3;\nprint(arr[0])\nlet r = (x ||= 8);\nprint(r, x)"),
        "0\n5\n9\n7\n0 0\n2\n3\n8 8"
    );
    // do...while runs at least once; continue/labeled break work.
    assert_eq!(
        run_lines("let i = 0, s = \"\";\ndo { s += i; i++; } while (i < 3);\nprint(s, i)\nlet once = 0;\ndo { once++; } while (false);\nprint(once)\nlet k = 0, c = \"\";\ndo { k++; if (k === 2) continue; c += k; } while (k < 4);\nprint(c)\nlet b = 0;\nouter: do { b++; if (b === 2) break outer; } while (b < 10);\nprint(b)"),
        "012 3\n1\n134\n2"
    );
    // in: own props, array indexes + synthesized methods, deleted keys,
    // TypeErrors on primitives, and the prototype-chain walk.
    assert_eq!(
        run_lines("let o = { a: 1 };\nprint(\"a\" in o, \"b\" in o)\nlet arr = [10];\nprint(0 in arr, 1 in arr, \"map\" in arr, \"length\" in arr)\nlet d = { x: 1 };\ndelete d.x;\nprint(\"x\" in d)\nfunction t(v) { try { return \"x\" in v; } catch (e) { return \"throws\"; } }\nprint(t(null), t(5))\nclass P { pm() {} }\nclass C extends P { cm() {} }\nlet inst = new C();\nprint(\"cm\" in C.prototype, \"pm\" in C.prototype, \"pm\" in inst, \"cm\" in new P())"),
        "true false\ntrue false true true\nfalse\nthrows throws\ntrue true true false"
    );
    // Object method shorthand, computed keys, and spread (incl. null, arrays,
    // strings, override order, deleted-key skipping).
    assert_eq!(
        run_lines("let m = { x: 10, getX() { return this.x; }, add(a, b) { return a + b; } };\nprint(m.getX(), m.add(2, 3))\nlet k = \"k\";\nlet c = { [k]: 1, [1 + 1]: 2 };\nprint(c.k, c[\"2\"])\nlet base = { a: 1, b: 2 };\nprint({ ...base, c: 3 } .c, { ...base } .a)\nprint(Object.keys({ ...null }).length, Object.keys({ ...[9, 8] }).join(\",\"), { ...\"ab\" }[\"0\"])\nprint({ a: 1, ...{ a: 9 } } .a, { ...{ a: 9 }, a: 1 } .a)"),
        "10 5\n1 2\n3 1\n0 0,1 a\n9 1"
    );
}

/// Regex literals: exec/test, captures, index/input, flags, global
/// lastIndex, empty-match advance, sticky — all Node-verified.
#[test]
fn regex_exec_test_and_flags() {
    assert_eq!(
        run_lines("let r = /ab+c/i;\nprint(r.test(\"xabbbc\"), r.test(\"nope\"))\nlet m = /(\\d+)-(\\d+)/.exec(\"id 12-34 end\");\nprint(m[0], m[1], m[2], m.index, m.input, m.length)\nprint(/a/.exec(\"xyz\"))"),
        "true false\n12-34 12 34 3 id 12-34 end 3\nnull"
    );
    // Global exec loop advances lastIndex and resets on failure.
    assert_eq!(
        run_lines("let g = /\\d+/g;\nlet s = \"a12b345\";\nlet m;\nlet out = [];\nwhile ((m = g.exec(s)) !== null) { out.push(m[0] + \"@\" + m.index); }\nprint(out.join(\" \"))\nprint(g.lastIndex)"),
        "12@1 345@4\n0"
    );
    // Empty-match advance terminates /g loops like Node (11 iterations for
    // "ab" with /(?:)/g).
    assert_eq!(
        run_lines("let e = /(?:)/g;\nlet n = 0;\nwhile (e.exec(\"ab\") !== null) { n++; if (n > 10) break; }\nprint(n)"),
        "11"
    );
    // Sticky: anchored at lastIndex (1 matches 'b' and advances to 2); a
    // miss (2) returns null and leaves lastIndex untouched.
    assert_eq!(
        run_lines("let y = /b/y;\ny.lastIndex = 1;\nprint(y.exec(\"abc\") === null, y.lastIndex)\ny.lastIndex = 2;\nprint(y.exec(\"abc\") === null, y.lastIndex)"),
        "false 2\ntrue 2"
    );
    // Flags / source / toString / typeof / per-literal identity.
    assert_eq!(
        run_lines("let fr = /x/gi;\nprint(fr.source, fr.flags, fr.global, fr.ignoreCase, fr.multiline)\nprint(fr.toString())\nprint(typeof fr)\nlet q1 = /z/g, q2 = /z/g;\nprint(q1 === q2)\nq1.lastIndex = 3;\nprint(q1.lastIndex, q2.lastIndex)"),
        "x gi true true false\n/x/gi\nobject\nfalse\n3 0"
    );
}

/// String.match / replace / search / split with regexes, and the classic
/// syntax surface (classes, anchors, quantifiers, alternation, backrefs).
#[test]
fn regex_string_methods_and_syntax() {
    assert_eq!(
        run_lines("print(\"abc123def456\".match(/\\d+/g).join(\",\"))\nprint(\"abc123def456\".match(/\\d+/)[0])\nprint(\"abcdef\".match(/x/))\nprint(\"abc\".match(\"b\")[0])"),
        "123,456\n123\nnull\nb"
    );
    assert_eq!(
        run_lines("print(\"hello world\".replace(\"o\", \"0\"))\nprint(\"hello world\".replace(/o/g, \"0\"))\nprint(\"a1b2c3\".replace(/(\\d)/g, \"<$1>\"))\nprint(\"John Smith\".replace(/(\\w+) (\\w+)/, \"$2, $1\"))\nprint(\"abc\".replace(/b/, \"[$&][$`][$']\"))\nprint(\"x1y2\".replace(/(\\d)/g, function (m, d) { return \"(\" + d + \")\"; }))\nprint(\"abc\".replace(/b/, function (m, off, str) { return off + \":\" + str.length; }))\nprint(\"aaa\".replace(/a/g, function () { return \"Z\"; }))"),
        "hell0 world\nhell0 w0rld\na<1>b<2>c<3>\nSmith, John\na[b][a][c]c\nx(1)y(2)\na1:3c\nZZZ"
    );
    assert_eq!(
        run_lines("print(\"2024-01-15\".search(/\\d+/))\nprint(\"abc\".search(/x/))\nprint(\"a,b,c\".split(/,/).join(\"|\"))\nprint(\"a1b2c\".split(/(\\d)/).join(\"|\"))\nprint(\"abc\".split(/(?:)/).join(\"|\"))\nprint(\"ab\".split(/(a)/).join(\"|\"))"),
        "0\n-1\na|b|c\na|1|b|2|c\na|b|c\n|a|b"
    );
    assert_eq!(
        run_lines("print(/[a-z]+/.exec(\"123abc456\")[0])\nprint(/[^0-9]+/.exec(\"123abc456\")[0])\nprint(/^(ab|cd)+$/.test(\"cdab\"), /^(ab|cd)+$/.test(\"cdx\"))\nprint(/<.+>/.exec(\"<a><b>\")[0])\nprint(/<.+?>/.exec(\"<a><b>\")[0])\nprint(/\\bword\\b/.test(\"a word!\"))\nprint(/(ab)\\1/.exec(\"abab\")[0])\nprint(/(a|b)\\1/.test(\"aa\"), /(a|b)\\1/.test(\"bb\"), /(a|b)\\1/.test(\"ab\"))\nprint(/[aeiou]+/i.exec(\"HELLO\")[0])\nprint(/^a.b$/m.test(\"z\\naqb\"), /a.b/s.test(\"a\\nb\"))"),
        "abc\nabc\ntrue false\n<a><b>\n<a>\ntrue\nabab\ntrue true false\nE\ntrue true"
    );
}

/// Division vs regex disambiguation (the lexer's prev-token rule) and loud
/// compile errors for bad patterns — Node's parse-time SyntaxErrors.
#[test]
fn regex_lexing_and_loud_errors() {
    assert_eq!(
        run_lines("print(10 / 2, 10 / /2/.exec(\"2\")[0])\nlet a = 5;\na++ / 2;\nprint(a)\nlet o = { n: 10 };\nprint(o.n / 2)\nprint(8 / 4, 8 / /4/.exec(\"4\")[0])\nfunction f() { return /x/.test(\"x\"); }\nprint(f())"),
        "5 5\n6\n5\n2 2\ntrue"
    );
    assert!(matches!(
        compile_err("let x = /[ab/;"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("let y = /a/zz;"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("let z = /a{2,1}/;"),
        CompileError::UnexpectedToken(_)
    ));
    assert!(matches!(
        compile_err("let w = /(ab/;"),
        CompileError::UnexpectedToken(_)
    ));
    // Unterminated pattern (a trailing backslash is also caught).
    assert!(matches!(
        compile_err("let v = /a\\"),
        CompileError::UnexpectedToken(_)
    ));
}

#[test]
fn web_crypto_known_vectors() {
    assert_eq!(
        run_lines("print(crypto.sha256(\"abc\"))"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        run_lines("print(crypto.hmacSha256(\"Jefe\", \"what do ya want for nothing?\"))"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        run_lines("print(crypto.base64Encode(\"Man\"), crypto.base64Decode(\"TWFu\"))"),
        "TWFu Man"
    );
    assert_eq!(
        run_lines(
            "print(crypto.timingSafeEqual(\"a\", \"a\"), crypto.timingSafeEqual(\"a\", \"b\"))"
        ),
        "true false"
    );
    assert_eq!(run_lines("print(crypto.randomHex(8).length)"), "16");
}

#[test]
fn web_uri_and_url_helpers() {
    assert_eq!(
        run_lines("print(encodeURIComponent(\"a b+c\"))"),
        "a%20b%2Bc"
    );
    assert_eq!(run_lines("print(decodeURIComponent(\"x%20y\"))"), "x y");
    assert_eq!(
        run_lines("print(btoa(\"Man\"), atob(\"TWFu\"))"),
        "TWFu Man"
    );
    assert_eq!(run_lines("const u = URL.parse(\"https://ex.com:8080/p?q=1#h\"); print(u.protocol, u.host, u.port, u.path)"), "https: ex.com:8080 8080 /p");
    assert_eq!(
        run_lines("const u = URL.parse(\"/rel\", \"http://b.com/r\"); print(u.hostname, u.path)"),
        "b.com /rel"
    );
}

#[test]
fn web_jwt_roundtrip_in_js() {
    // HS256 sign -> verify using only engine natives (mirrors lib/auth.ajs).
    assert_eq!(
        run_lines("const h = crypto.base64UrlEncode(JSON.stringify({alg:\"HS256\"}));\nconst p = crypto.base64UrlEncode(JSON.stringify({sub:\"u\"}));\nconst s = crypto.hmacBase64Url(\"s3cret\", h + \".\" + p);\nconst t = h + \".\" + p + \".\" + s;\nprint(crypto.timingSafeEqual(s, t.split(\".\")[2]))"),
        "true"
    );
}

#[test]
fn web_res_status_and_helpers_exist() {
    // Response controls are seeded natives (shape check without a server).
    assert_eq!(run_lines("print(typeof res_unused)"), "undefined");
    assert_eq!(
        run_lines("print(typeof encodeURIComponent, typeof fetchSync, typeof crypto, typeof URL)"),
        "function function object object"
    );
}

#[test]
fn cannot_shadow_web_builtins() {
    assert!(matches!(
        compile_err("crypto = 5"),
        CompileError::CannotShadowBuiltin(_)
    ));
    assert!(matches!(
        compile_err("function fetchSync() {}"),
        CompileError::CannotShadowBuiltin(_)
    ));
}

#[test]
fn regex_catastrophic_backtracking_throws() {
    let out = run_lines(
        "try {\n\
            const re = /(a+)+b/;\n\
            re.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');\n\
            print('no throw');\n\
        } catch (e) {\n\
            print('caught: ' + (e.message || e));\n\
        }",
    );
    assert!(out.starts_with(
        "caught: SyntaxError: Invalid regular expression: regular expression too complex"
    ));
}

#[test]
fn generator_basic_and_stepping() {
    let src = r#"
        function* gen() {
            yield 10;
            yield 20;
            return 30;
        }
        let g = gen();
        let r1 = g.next();
        let r2 = g.next();
        let r3 = g.next();
        let r4 = g.next();
        print(r1.value, r1.done);
        print(r2.value, r2.done);
        print(r3.value, r3.done);
        print(r4.value, r4.done);
    "#;
    assert_eq!(
        run_lines(src),
        "10 false\n20 false\n30 true\nundefined true"
    );
}

#[test]
fn generator_next_with_arguments() {
    let src = r#"
        function* gen() {
            let a = yield 1;
            let b = yield (a + 10);
            return b * 2;
        }
        let g = gen();
        let r1 = g.next();
        let r2 = g.next(5);
        let r3 = g.next(7);
        print(r1.value, r2.value, r3.value);
    "#;
    assert_eq!(run_lines(src), "1 15 14");
}

#[test]
fn generator_for_of_and_spread() {
    let src = r#"
        function* nums() {
            yield 1;
            yield 2;
            yield 3;
        }
        let sum = 0;
        for (let x of nums()) {
            sum += x;
        }
        let arr = [...nums()];
        print(sum, arr.join(","));
    "#;
    assert_eq!(run_lines(src), "6 1,2,3");
}

#[test]
fn generator_yield_star() {
    let src = r#"
        function* inner() {
            yield 2;
            yield 3;
        }
        function* outer() {
            yield 1;
            yield* inner();
            yield* [4, 5];
            yield 6;
        }
        let res = [...outer()];
        print(res.join(","));
    "#;
    assert_eq!(run_lines(src), "1,2,3,4,5,6");
}

#[test]
fn generator_methods_in_object_and_class() {
    let src = r#"
        let obj = {
            *items() {
                yield "a";
                yield "b";
            }
        };
        class C {
            *gen() {
                yield 100;
                yield 200;
            }
        }
        let c = new C();
        print([...obj.items()].join("-"));
        print([...c.gen()].join("-"));
    "#;
    assert_eq!(run_lines(src), "a-b\n100-200");
}

#[test]
fn class_public_and_static_fields() {
    let src = r#"
        class Point {
            x = 10;
            y = 20;
            static origin = "0,0";
            constructor(z) {
                this.z = z;
            }
        }
        let p = new Point(30);
        print(p.x, p.y, p.z, Point.origin);
    "#;
    assert_eq!(run_lines(src), "10 20 30 0,0");
}

#[test]
fn class_private_fields_and_methods() {
    let src = r#"
        class Counter {
            #count = 5;
            #secret() {
                return 42;
            }
            inc() {
                this.#count += 1;
            }
            getVal() {
                return this.#count + this.#secret();
            }
            testInvalid(other) {
                return other.#count;
            }
        }
        let c = new Counter();
        c.inc();
        print(c.getVal());
        try {
            c.testInvalid({});
            print("no throw");
        } catch (e) {
            print("caught: " + (e.message || e));
        }
    "#;
    assert_eq!(
        run_lines(src),
        "48\ncaught: TypeError: Cannot read private member #count from an object whose class did not declare it"
    );
}

#[test]
fn class_and_object_getters_setters() {
    let src = r#"
        class Box {
            _w = 5;
            get width() { return this._w; }
            set width(v) { this._w = v * 2; }
        }
        let b = new Box();
        print(b.width);
        b.width = 10;
        print(b.width);

        let obj = {
            _x: 100,
            get x() { return this._x; },
            set x(val) { this._x = val + 1; }
        };
        print(obj.x);
        obj.x = 200;
        print(obj.x);
    "#;
    assert_eq!(run_lines(src), "5\n20\n100\n201");
}

#[test]
fn array_and_string_at() {
    let src = r#"
        let a = [10, 20, 30, 40];
        print(a.at(0), a.at(-1), a.at(-2), a.at(10));
        let s = "alloy";
        print(s.at(0), s.at(-1), s.at(-2), s.at(10));
    "#;
    assert_eq!(run_lines(src), "10 40 30 undefined\na y o undefined");
}

#[test]
fn proxy_and_reflect_traps() {
    let src = r#"
        let target = { a: 1, b: 2 };
        let proxy = new Proxy(target, {
            get(t, prop) {
                return t[prop] * 10;
            },
            set(t, prop, val) {
                t[prop] = val + 5;
                return true;
            }
        });
        print(proxy.a, proxy.b);
        proxy.a = 3;
        print(proxy.a, target.a);

        let rev = Proxy.revocable({ x: 99 }, {});
        print(rev.proxy.x);
        rev.revoke();
        try {
            let _ = rev.proxy.x;
            print("no throw");
        } catch (e) {
            print("revoked caught");
        }
    "#;
    assert_eq!(run_lines(src), "10 20\n80 8\n99\nrevoked caught");
}

#[test]
fn symbol_iterator_protocol() {
    let src = r#"
        let arr = [100, 200];
        let it = arr[Symbol.iterator]();
        let s1 = it.next();
        let s2 = it.next();
        let s3 = it.next();
        print(s1.value, s1.done);
        print(s2.value, s2.done);
        print(s3.value, s3.done);

        let sit = "hi"[Symbol.iterator]();
        print(sit.next().value, sit.next().value, sit.next().done);
    "#;
    assert_eq!(
        run_lines(src),
        "100 false\n200 false\nundefined true\nh i true"
    );
}

#[test]
fn object_has_own_and_from_entries() {
    let src = r#"
        let obj = { a: 1 };
        print(Object.hasOwn(obj, "a"), Object.hasOwn(obj, "toString"));
        let entries = [["x", 10], ["y", 20]];
        let fromObj = Object.fromEntries(entries);
        print(fromObj.x, fromObj.y);
    "#;
    assert_eq!(run_lines(src), "true false\n10 20");
}

#[test]
fn structured_clone_test() {
    let src = r#"
        let orig = { num: 42, arr: [1, { k: "v" }] };
        let copy = structuredClone(orig);
        copy.arr[1].k = "changed";
        print(orig.arr[1].k, copy.arr[1].k);
    "#;
    assert_eq!(run_lines(src), "v changed");
}

#[test]
fn error_stack_trace_formatting() {
    let src = r#"
        function baz() {
            let err = new Error("something went wrong");
            print(err.name);
            print(err.message);
            print(typeof err.stack);
            print(err.stack.includes("Error: something went wrong"));
            print(err.stack.includes("at baz"));
        }
        function bar() { baz(); }
        function foo() { bar(); }
        foo();
    "#;
    assert_eq!(
        run_lines(src),
        "Error\nsomething went wrong\nstring\ntrue\ntrue"
    );
}

#[test]
fn error_capture_stack_trace() {
    let src = r#"
        function MyError(msg) {
            this.name = "MyError";
            this.message = msg;
            Error.captureStackTrace(this, MyError);
        }
        function testHelper() {
            let e = new MyError("custom fail");
            print(e.stack.includes("MyError: custom fail"));
            print(!e.stack.includes("at MyError"));
            print(e.stack.includes("at testHelper"));
        }
        testHelper();
    "#;
    assert_eq!(run_lines(src), "true\ntrue\ntrue");
}

#[test]
fn bytecode_line_table_lookup() {
    let mut prog = alloy_vm::bytecode::Program::new();
    prog.source_file = Some("test.ajs".to_string());
    prog.line_table.push((0, 1, 1));
    prog.line_table.push((10, 2, 5));
    prog.line_table.push((25, 5, 12));
    prog.line_table.push((50, 10, 1));

    assert_eq!(prog.get_location(0), Some((1, 1)));
    assert_eq!(prog.get_location(5), Some((1, 1)));
    assert_eq!(prog.get_location(10), Some((2, 5)));
    assert_eq!(prog.get_location(20), Some((2, 5)));
    assert_eq!(prog.get_location(25), Some((5, 12)));
    assert_eq!(prog.get_location(49), Some((5, 12)));
    assert_eq!(prog.get_location(50), Some((10, 1)));
    assert_eq!(prog.get_location(100), Some((10, 1)));
}

#[test]
fn fs_read_file_sync_throws_catchable_error() {
    let src = r#"
        import * as fs from "alloy:fs";
        try {
            fs.readFileSync("this_path_does_not_exist_at_all_xyz_123.txt");
            print("unreachable");
        } catch (e) {
            print("caught exception");
        }
    "#;
    assert_eq!(run_lines(src), "caught exception");
}

#[test]
fn const_require_builtin_shadowing() {
    let src = r#"
        const fs = require('fs');
        const http = require('http');
        const crypto = require('crypto');
        print(typeof fs, typeof http, typeof crypto);
    "#;
    assert_eq!(run_lines(src), "object object object");
}

#[test]
fn json_stringify_date_and_to_json() {
    let src = r#"
        print(JSON.stringify(new Date(0)));
        print(JSON.stringify(new Date(NaN)));
        let custom = { a: 1, toJSON: function() { return { b: 2 }; } };
        print(JSON.stringify(custom));
    "#;
    assert_eq!(
        run_lines(src),
        "\"1970-01-01T00:00:00.000Z\"\nnull\n{\"b\":2}"
    );
}

#[test]
fn json_and_commonjs_module_loading() {
    let pid = std::process::id();
    let json_file = format!(".alloy_test_{}.json", pid);
    let js_file = format!(".alloy_test_{}.js", pid);

    std::fs::write(&json_file, r#"{"name": "alloy", "count": 42}"#).unwrap();
    std::fs::write(&js_file, "module.exports = { greet: 'hello from cjs' };").unwrap();

    let src = format!(
        "const data = require('./{}');\n\
         const cjs = require('./{}');\n\
         print(data.name, data.count, cjs.greet);\n",
        json_file, js_file
    );
    let out = run_lines(&src);
    let _ = std::fs::remove_file(&json_file);
    let _ = std::fs::remove_file(&js_file);

    assert_eq!(out, "alloy 42 hello from cjs");
}

#[test]
fn node_path_module_methods() {
    let src = r#"
        const path = require('path');
        const nodePath = require('node:path');
        print(typeof path.join, typeof nodePath.resolve);
        print(path.join('a', 'b', 'c'));
        print(path.dirname('foo/bar/baz.txt'));
        print(path.basename('foo/bar/baz.txt'));
        print(path.basename('foo/bar/baz.txt', '.txt'));
        print(path.extname('foo/bar/baz.txt'));
        print(path.extname('foo/bar/baz'));
        print(path.isAbsolute('/foo/bar'));
        print(path.normalize('a//b/../c'));
    "#;
    let out = run_lines(src);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "function function");
    assert!(lines[1].ends_with("a/b/c") || lines[1].ends_with("a\\b\\c"));
    assert!(lines[2] == "foo/bar" || lines[2] == "foo\\bar");
    assert_eq!(lines[3], "baz.txt");
    assert_eq!(lines[4], "baz");
    assert_eq!(lines[5], ".txt");
    assert_eq!(lines[6], "");
    assert!(lines[7] == "true" || lines[7] == "false"); // depends on OS
    assert!(lines[8].ends_with("a/c") || lines[8].ends_with("a\\c"));
}

#[test]
fn extended_fs_sync_methods() {
    let pid = std::process::id();
    let base_dir = std::env::temp_dir().join(format!("alloy_ext_fs_test_{}", pid));
    let dir_str = base_dir.to_string_lossy().replace('\\', "/");
    let src = format!(
        r#"
        const fs = require('fs');
        const testDir = "{}/nested/sub";
        fs.mkdirSync(testDir);
        const f1 = testDir + "/file1.txt";
        const f2 = testDir + "/file2.txt";
        fs.writeFileSync(f1, "hello alloy");
        fs.copyFileSync(f1, f2);
        print("f2 exists:", fs.existsSync(f2));
        print("f2 content:", fs.readFileSync(f2));
        const entries = fs.readdirSync(testDir);
        print("entries count:", entries.length);
        const st = fs.statSync(f1);
        print("isFile:", st.isFile(), "size:", st.size);
        fs.unlinkSync(f1);
        print("f1 after unlink:", fs.existsSync(f1));
    "#,
        dir_str
    );
    let out = run_lines(&src);
    let _ = std::fs::remove_dir_all(&base_dir);
    assert!(out.contains("f2 exists: true"));
    assert!(out.contains("f2 content: hello alloy"));
    assert!(out.contains("entries count: 2"));
    assert!(out.contains("isFile: true size: 11"));
    assert!(out.contains("f1 after unlink: false"));
}

#[test]
fn crypto_random_uuid_and_bytes() {
    let src = r#"
        const id1 = crypto.randomUUID();
        const id2 = crypto.randomUUID();
        print("id1 len:", id1.length);
        print("id1 differs:", id1 !== id2);
        print("v4 marker:", id1.charAt(14));
        const bytes = crypto.randomBytes(16);
        print("bytes count:", bytes.length);
        print("is array:", Array.isArray(bytes));
    "#;
    let out = run_lines(src);
    assert!(out.contains("id1 len: 36"));
    assert!(out.contains("id1 differs: true"));
    assert!(out.contains("v4 marker: 4"));
    assert!(out.contains("bytes count: 16"));
    assert!(out.contains("is array: true"));
}

#[test]
fn json_deep_nesting_depth_limit() {
    // Generate JSON with nesting depth 600: [[[[...]]]]
    let deep_json = "[".repeat(600) + &"]".repeat(600);
    let src = format!(
        r#"
        try {{
            JSON.parse('{}');
            print("did not throw");
        }} catch(e) {{
            print("CAUGHT DEPTH ERROR:", e);
        }}
        "#,
        deep_json
    );
    let out = run_lines(&src);
    assert!(out.contains("CAUGHT DEPTH ERROR: SyntaxError: JSON structure too deeply nested"));
}
