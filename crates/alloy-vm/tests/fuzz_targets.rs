use alloy_core::regex::{compile_from_str, search};
use alloy_vm::bytecode::Program;
use alloy_vm::compiler::lexer::Lexer;
use alloy_vm::compiler::Compiler;
use alloy_vm::vm::fuzz::{dechunk, decode_spawn_value, parse_http_request_full, parse_json_str};

// Simple LCG pseudo-random generator for deterministic, reproducible fuzzing without external dependencies
struct FuzzRng {
    state: u64,
}

impl FuzzRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.state >> 32) as u32
    }

    fn gen_bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push((self.next_u32() & 0xFF) as u8);
        }
        out
    }

    fn gen_string(&mut self, len: usize) -> String {
        let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 \t\n\r+-*/=<>!~?#$:;,.{}()[]`'\"\\";
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            let idx = (self.next_u32() as usize) % chars.len();
            s.push(chars[idx] as char);
        }
        s
    }
}

// ---------------------------------------------------------------------------
// 1. Fuzz Target: Lexer & Tokenizer
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_lexer() {
    let mut rng = FuzzRng::new(0x1337_CAFE);

    // Corpus of tricky manual edge cases
    let corpus = [
        "",
        "`unclosed template",
        "`template with ${42} and more ${`nested ${x}`}`",
        "`template with ${",
        "\"unclosed string",
        "'unclosed single quote",
        "/* unclosed block comment",
        "// line comment\n42",
        "0x",
        "0b",
        "0o",
        "1.2.3.4.5",
        "9999999999999999999999999999999999999999999999999999999999999999999",
        "1e+999999",
        "1e-999999",
        "/regex/g",
        "/unclosed regex",
        "/regex with [class] and \\/ escape/i",
        "#privateField",
        "#",
        "...",
        "..",
        "?.[]",
        "??=",
        "&&=",
        "||=",
        "**=",
        "\\u{0000}",
        "\\u{10FFFF}",
        "\\u{99999999}",
    ];

    for case in corpus {
        let mut lexer = Lexer::new(case);
        let _ = lexer.tokenize();
    }

    // 1000 iterations of randomized fuzz input
    for _ in 0..1000 {
        let len = (rng.next_u32() % 256) as usize;
        let input = rng.gen_string(len);
        let mut lexer = Lexer::new(&input);
        let _ = lexer.tokenize();
    }
}

// ---------------------------------------------------------------------------
// 2. Fuzz Target: Parser & AST Compiler
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_parser() {
    let mut rng = FuzzRng::new(0xDEAD_BEEF);

    // Pathological nesting and syntax cases
    let mut deep_parens = String::new();
    for _ in 0..200 {
        deep_parens.push('(');
    }
    deep_parens.push_str("42");
    for _ in 0..200 {
        deep_parens.push(')');
    }

    let mut deep_arrays = String::new();
    for _ in 0..200 {
        deep_arrays.push('[');
    }
    deep_arrays.push('1');
    for _ in 0..200 {
        deep_arrays.push(']');
    }

    let corpus = [
        deep_parens,
        deep_arrays,
        "function f(a = 1, [b, ...c], { d: e = 2 }) { return a + b; }".to_string(),
        "try { throw 1; } catch (e) { try {} finally {} } finally {}".to_string(),
        "class A extends B { #x = 1; get x() { return this.#x; } set x(v) { this.#x = v; } }"
            .to_string(),
        "async function* gen() { yield* [1, 2, await 3]; }".to_string(),
        "for (const x of y) { break; continue; }".to_string(),
        "export default class extends null {}".to_string(),
        "import { a as b, c } from 'foo';".to_string(),
        "(((((((((())))))))))".to_string(),
        "switch(x) { case 1: case 2: default: }".to_string(),
        "let [,,,...rest] = arr;".to_string(),
        "let { a, b: [c, { d }] } = obj;".to_string(),
    ];

    for case in &corpus {
        let _ = Compiler::compile_source(case);
    }

    // Randomized code combinations
    for _ in 0..1000 {
        let len = (rng.next_u32() % 200) as usize;
        let input = rng.gen_string(len);
        let _ = Compiler::compile_source(&input);
    }
}

// ---------------------------------------------------------------------------
// 3. Fuzz Target: Bytecode Deserializer (Program::from_bytes)
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_bytecode() {
    let mut rng = FuzzRng::new(0xFEED_FACE);

    // 1. Generate valid bytecode from source
    let valid_prog = Compiler::compile_source("let x = 1 + 2; print(x);").unwrap();
    let valid_bytes = valid_prog.to_bytes().unwrap();

    // Roundtrip verification
    assert!(Program::from_bytes(&valid_bytes).is_ok());

    // Fuzz by truncating at every byte boundary
    for i in 0..valid_bytes.len() {
        let truncated = &valid_bytes[..i];
        let _ = Program::from_bytes(truncated);
    }

    // Fuzz by bit-flipping random bytes in valid bytecode
    for _ in 0..1000 {
        let mut corrupted = valid_bytes.clone();
        let flip_idx = (rng.next_u32() as usize) % corrupted.len();
        corrupted[flip_idx] ^= (rng.next_u32() & 0xFF) as u8;
        let _ = Program::from_bytes(&corrupted);
    }

    // Fuzz completely random byte buffers
    for _ in 0..1000 {
        let len = (rng.next_u32() % 512) as usize;
        let random_data = rng.gen_bytes(len);
        let _ = Program::from_bytes(&random_data);
    }
}

// ---------------------------------------------------------------------------
// 4. Fuzz Target: JSON Parser (parse_json_str)
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_json() {
    let mut rng = FuzzRng::new(0xCAFE_BABE);

    let corpus = [
        "",
        "null",
        "true",
        "false",
        "42",
        "-42",
        "0.12345",
        "1e10",
        "-1.5e-20",
        "\"hello world\"",
        "\"escapes: \\\" \\\\ \\/ \\b \\f \\n \\r \\t \\u0041\"",
        "\"surrogate pair: \\uD83D\\uDE00\"",
        "\"unpaired high: \\uD800\"",
        "\"unpaired low: \\uDC00\"",
        "[]",
        "[1, 2, [3, 4, [5]]]",
        "{}",
        "{\"a\": 1, \"b\": [true, null, \"str\"], \"c\": {\"d\": 42}}",
        // Invalid edge cases
        "{",
        "[",
        "{ \"a\": }",
        "{\"a\": 1,}",
        "[1, 2, ]",
        "0123",
        "-0123",
        "+42",
        "1.",
        ".5",
        "\"unclosed string",
        "\"bad escape: \\x41\"",
        "\"bad unicode: \\u00\"",
        "{\"key\" \"missing colon\"}",
        "NaN",
        "Infinity",
    ];

    for case in corpus {
        let _ = parse_json_str(case);
    }

    // Randomized fuzz inputs
    for _ in 0..1000 {
        let len = (rng.next_u32() % 256) as usize;
        let s = rng.gen_string(len);
        let _ = parse_json_str(&s);
    }
}

// ---------------------------------------------------------------------------
// 5. Fuzz Target: Regex Engine (AlloyRegex)
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_regex() {
    let mut rng = FuzzRng::new(0xBEEF_CAFE);

    let patterns = [
        "abc",
        "^abc$",
        "a*b+c?",
        "(foo|bar)+",
        "[a-z0-9_-]",
        "[^a-zA-Z]",
        "\\d+\\s+\\w+",
        "(?:non-capturing)",
        "(a+)+$", // Catastrophic backtracking candidate
        "([a-z]+)*[0-9]+",
        // Malformed patterns
        "[a-z",
        "(abc",
        "*abc",
        "?abc",
        "+abc",
        "{1,2}",
        "\\",
    ];

    let flags_list = [
        "",
        "g",
        "i",
        "m",
        "u",
        "s",
        "gi",
        "gims",
        "invalid_flags",
        "xyz",
    ];

    for pat in patterns {
        for flags in flags_list {
            if let Ok(prog) = compile_from_str(pat, flags) {
                let chars: Vec<char> = "The quick brown fox jumps over 42 lazy dogs."
                    .chars()
                    .collect();
                let _ = search(&prog, &chars, 0);
                let chars2: Vec<char> = "".chars().collect();
                let _ = search(&prog, &chars2, 0);
                let chars3: Vec<char> = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaab".chars().collect();
                let _ = search(&prog, &chars3, 0);
            }
        }
    }

    // Randomized fuzz inputs
    for _ in 0..500 {
        let pat_len = (rng.next_u32() % 32) as usize;
        let pat = rng.gen_string(pat_len);
        let flags = if rng.next_u32() & 1 == 0 { "g" } else { "i" };
        if let Ok(prog) = compile_from_str(&pat, flags) {
            let test_input = rng.gen_string(64);
            let chars: Vec<char> = test_input.chars().collect();
            let _ = search(&prog, &chars, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Fuzz Target: HTTP Parser & Chunked Decoder
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_http() {
    let mut rng = FuzzRng::new(0xABCD_EF01);

    let valid_requests = [
        b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
        b"POST /api/data HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhello"
            .as_slice(),
        b"GET /test?query=1 HTTP/1.1\r\nUser-Agent: Alloy\r\nAccept: */*\r\n\r\n".as_slice(),
    ];

    for req in valid_requests {
        let s = String::from_utf8_lossy(req);
        let _ = parse_http_request_full(&s);
    }

    // Dechunk edge cases
    let valid_chunks = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
    let _ = dechunk(valid_chunks);

    let bad_chunks = [
        b"".as_slice(),
        b"0\r\n\r\n".as_slice(),
        b"FFFFFFFF\r\nhello\r\n0\r\n\r\n".as_slice(),
        b"-5\r\nhello\r\n0\r\n\r\n".as_slice(),
        b"5\r\nhel".as_slice(),
        b"xyz\r\n".as_slice(),
    ];

    for bc in bad_chunks {
        let _ = dechunk(bc);
    }

    // Fuzz with random byte sequences
    for _ in 0..1000 {
        let len = (rng.next_u32() % 512) as usize;
        let data = rng.gen_bytes(len);
        let s = String::from_utf8_lossy(&data);
        let _ = parse_http_request_full(&s);
        let _ = dechunk(&data);
    }
}

// ---------------------------------------------------------------------------
// 7. Fuzz Target: IPC Spawn Value Deserializer
// ---------------------------------------------------------------------------
#[test]
fn test_fuzz_ipc() {
    let mut heap = alloy_core::heap::ArenaHeap::new(65536);
    let _g = alloy_core::heap::HeapGuard::set(&mut heap);
    let mut rng = FuzzRng::new(0x9876_5432);

    // Empty and single-byte edge cases
    for tag in 0..=255u8 {
        let data = vec![tag];
        let mut pos = 0;
        let _ = decode_spawn_value(&data, &mut pos);
    }

    // Random byte buffers of various sizes
    for _ in 0..2000 {
        let len = (rng.next_u32() % 256) as usize;
        let data = rng.gen_bytes(len);
        let mut pos = 0;
        let _ = decode_spawn_value(&data, &mut pos);
    }
}
