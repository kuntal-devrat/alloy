use alloy_core::arena::Arena;
use alloy_vm::bytecode::Program;

use alloy_vm::compiler::Compiler;
use alloy_vm::vm::Vm;
use std::env;
use std::fs;
use std::time::Instant;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("alloy runtime v0.1.0");
        eprintln!("Usage: alloy <script.ajs|script.js|script.ax>");
        eprintln!("       alloy --bench <script.ajs>");
        eprintln!("       alloy --disasm <script.ajs|script.js|script.ax>");
        eprintln!("       alloy --emit-ax <script.ajs> <out.ax>");
        return;
    }

    match args[1].as_str() {
        "--version" | "-v" => {
            println!("alloy 0.1.0");
        }
        "--help" | "-h" => {
            println!("alloy - A hyper-optimized polyglot systems runtime");
            println!();
            println!("Usage:");
            println!("  alloy <script.ajs>           Execute an alloy source file (.ajs is");
            println!("                                the native extension; plain .js is also");
            println!("                                accepted for compatibility)");
            println!("  alloy <script.ax>             Execute precompiled bytecode");
            println!("  alloy --emit-ax <in> <out>    Compile source to .ax bytecode");
            println!("  alloy --bench <script.ajs>    Benchmark execution");
            println!("  alloy --disasm <file>         Disassemble source or bytecode");
            println!("  alloy --version               Print version");
        }
        "--emit-ax" => {
            if args.len() < 4 {
                eprintln!("Usage: alloy --emit-ax [--module] <script.ajs> <out.ax>");
                return;
            }
            let (module, input, output) = if args[2] == "--module" {
                (true, &args[3], &args[4])
            } else {
                (false, &args[2], &args[3])
            };
            emit_ax(input, output, module);
        }
        "--bench" => {
            if args.len() < 3 {
                eprintln!("Usage: alloy --bench <script.ajs>");
                return;
            }
            bench_file(&args[2]);
        }
        "--disasm" => {
            if args.len() < 3 {
                eprintln!("Usage: alloy --disasm <script.ajs|script.js|script.ax>");
                return;
            }
            disasm_file(&args[2]);
        }
        "--repl" => {
            run_repl();
        }
        _ => {
            run_file(&args[1]);
        }
    }
}

fn load_program(path: &str) -> Program {
    if path.ends_with(".ax") {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error: cannot read '{}': {}", path, e);
                std::process::exit(1);
            }
        };
        match Program::from_bytes(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: invalid .ax file '{}': {}", path, e);
                std::process::exit(1);
            }
        }
    } else {
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read '{}': {}", path, e);
                std::process::exit(1);
            }
        };
        match Compiler::compile_source(&source) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("compile error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn run_file(path: &str) {
    let start = Instant::now();
    let program = load_program(path);
    let compile_time = start.elapsed();

    let mut vm = Vm::new(program);
    // Relative `require('./x.ajs')` resolves against the script's own
    // directory (Node semantics), not the process cwd.
    vm.set_script_path(path);
    let result = vm.run();
    if let Some(err) = vm.take_error() {
        eprintln!("uncaught exception: {}", err);
        std::process::exit(1);
    }

    let total = start.elapsed();
    if env::var("ALLOY_TRACE").is_ok() {
        eprintln!("compile: {:?} | total: {:?} | result: {}", compile_time, total, result);
    }
}

fn emit_ax(input: &str, output: &str, module: bool) {
    let source = match fs::read_to_string(input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read '{}': {}", input, e);
            std::process::exit(1);
        }
    };
    // `--module`: compile with module semantics (top-level globals, exports
    // recorded, is_module flag) so `require('./x.ax')` can load it. Default
    // is a main-script program (require refuses it — a loud error instead of
    // silently mis-executing against the wrong frame base).
    let program = if module {
        match Compiler::compile_module(&source) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("compile error: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match Compiler::compile_source(&source) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("compile error: {}", e);
                std::process::exit(1);
            }
        }
    };
    let bytes = match program.to_bytes() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("serialize error: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = fs::write(output, &bytes) {
        eprintln!("error: cannot write '{}': {}", output, e);
        std::process::exit(1);
    }
    println!("wrote {} bytes to {}", bytes.len(), output);
}

fn bench_file(path: &str) {
    let iterations = 1000;

    let compile_start = Instant::now();
    let program = load_program(path);
    let compile_time = compile_start.elapsed();

    let exec_start = Instant::now();
    for _ in 0..iterations {
        // Each VM needs a program whose constants live in their own arena.
        let prog = match program.deep_clone() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("error: program not serializable for --bench");
                std::process::exit(1);
            }
        };
        let mut vm = Vm::new(prog);
        vm.run();
    }
    let exec_time = exec_start.elapsed();

    println!("alloy benchmark: {}", path);
    println!("  iterations:  {}", iterations);
    println!("  compile:     {:?}", compile_time);
    println!("  total exec:  {:?}", exec_time);
    println!("  per exec:    {:?}", exec_time / iterations);
    println!("  execs/sec:   {:.0}", iterations as f64 / exec_time.as_secs_f64());
}

fn disasm_file(path: &str) {
    let program = load_program(path);

    println!("=== Raw Bytecode ===");
    for (i, b) in program.bytecode.iter().enumerate() {
        print!("{:02x} ", b);
        if (i + 1) % 16 == 0 { println!(); }
    }
    println!();
    println!("=== Constants ===");
    for (i, c) in program.constants.iter().enumerate() {
        println!("  {}: {}", i, c);
    }
    println!();
    println!("=== Bytecode ===");
    program.disassemble();
}

fn run_repl() {
    use std::io::{self, Write};

    println!("alloy REPL v0.1.0");
    println!("Type 'exit' to quit, 'help' for commands");
    println!();

    let arena = Arena::new(64 * 1024);
    let mut repl_vm: Option<Vm> = None;
    // Accumulated input across lines; evaluated as one program once the
    // brackets balance (loops, functions, and try blocks can span lines).
    let mut buf = String::new();

    loop {
        print!("{}", if buf.is_empty() { "alloy> " } else { "...   " });
        io::stdout().flush().unwrap();

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {}", e);
                break;
            }
        }
        let line = line.trim().to_string();

        // Commands only apply at the start of an input; while a construct is
        // open they are just more source text.
        if buf.is_empty() {
            match line.as_str() {
                "exit" | "quit" => break,
                "help" => {
                    println!("Commands:");
                    println!("  help     Show this help");
                    println!("  arena    Show arena memory stats");
                    println!("  exit     Quit the REPL");
                    println!();
                    println!("Or type any JavaScript expression to evaluate it.");
                    continue;
                }
                "arena" => {
                    println!("Arena: {}/{} bytes used", arena.used(), arena.capacity());
                    continue;
                }
                "" => continue,
                _ => {}
            }
        }

        buf.push_str(&line);
        buf.push('\n');
        if !Compiler::input_is_complete(&buf) {
            continue;
        }
        let src = std::mem::take(&mut buf);

        let start = Instant::now();
        // Top-level declarations become globals and the VM persists between
        // inputs, so `let x = 5` is visible to later inputs.
        match Compiler::compile_source_with_mode(&src, true, false) {
            Ok(program) => {
                match &mut repl_vm {
                    Some(vm) => vm.set_program(program),
                    None => repl_vm = Some(Vm::new(program)),
                }
                let result = repl_vm.as_mut().unwrap().run();
                let elapsed = start.elapsed();
                if let Some(err) = repl_vm.as_mut().unwrap().take_error() {
                    eprintln!("uncaught exception: {}", err);
                } else {
                    println!("=> {} ({:?})", result, elapsed);
                }
            }
            Err(e) => {
                eprintln!("error: {}", e);
            }
        }
    }
}
