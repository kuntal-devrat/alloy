use alloy_vm::compiler::{strip_typescript, Compiler};
use alloy_vm::permissions::Permissions;
use alloy_vm::vm::Vm;

#[test]
fn test_typescript_stripping_basic_types() {
    let ts_source = r#"
        interface User {
            id: number;
            name: string;
            tags: string[];
        }

        type ID = string | number;

        function greet(user: User, times: number): string {
            let greeting: string = "Hello " + user.name;
            return greeting;
        }

        const person = { name: "Alice", id: 1 as ID };
        const result: string = greet(person as any, 2);
    "#;

    let js_code = strip_typescript(ts_source);
    let program = match Compiler::compile_source(&js_code) {
        Ok(p) => p,
        Err(e) => panic!("compile error on:\n{}\nError: {}", js_code, e),
    };
    let mut vm = Vm::new(program);
    vm.run();
    if let Some(err) = vm.take_error() {
        panic!("vm run error on:\n{}\nError: {}", js_code, err);
    }
}

#[test]
fn test_typescript_preserves_object_literals_and_ternary() {
    let ts_source = r#"
        const isTrue: boolean = true;
        const val = isTrue ? 10 : 20;
        const obj = { a: 1, b: 2, c: "three" };
        let res = obj.a + val;
    "#;

    let js_code = strip_typescript(ts_source);
    let program = match Compiler::compile_source(&js_code) {
        Ok(p) => p,
        Err(e) => panic!("compile error on:\n{}\nError: {}", js_code, e),
    };
    let mut vm = Vm::new(program);
    vm.run();
    if let Some(err) = vm.take_error() {
        panic!("vm run error on:\n{}\nError: {}", js_code, err);
    }

    let inline_ts = "interface Config { port: number; host: string; } const c: Config = { port: 8080, host: 'localhost' };";
    let stripped_inline = strip_typescript(inline_ts);
    let _ = Compiler::compile_source(&stripped_inline).expect("compile error on inline config");
}

#[test]
fn test_permissions_sandbox_blocks_fs() {
    let code = r#"
        const fs = require('fs');
        fs.readFileSync('/etc/passwd');
    "#;

    let program = Compiler::compile_source(code).expect("compile error");
    let mut vm = Vm::new(program).with_permissions(Permissions::sandboxed());
    vm.run();
    let err = vm.take_error().expect("Expected error when accessing fs in sandbox");
    assert!(
        err.to_string().contains("PermissionDenied"),
        "Expected PermissionDenied error, got: {}",
        err
    );
}

#[test]
fn test_permissions_sandbox_blocks_fetch() {
    let code = r#"
        fetchSync('https://example.com');
    "#;

    let program = Compiler::compile_source(code).expect("compile error");
    let mut vm = Vm::new(program).with_permissions(Permissions::sandboxed());
    vm.run();
    let err = vm.take_error().expect("Expected error when fetching in sandbox");
    assert!(
        err.to_string().contains("PermissionDenied"),
        "Expected PermissionDenied error, got: {}",
        err
    );
}

#[test]
fn test_permissions_allow_read_permits_reading() {
    let code = r#"
        const fs = require('fs');
        const exists = fs.existsSync('.');
    "#;

    let program = Compiler::compile_source(code).expect("compile error");
    let perms = Permissions::sandboxed().allow_read(true);
    let mut vm = Vm::new(program).with_permissions(perms);
    vm.run();
    assert!(vm.take_error().is_none());
}

#[test]
fn test_permissions_sandbox_blocks_python() {
    let perms = Permissions::sandboxed();
    let err = perms.check_python().expect_err("sandboxed must disallow python");
    assert!(err.contains("Python access is not permitted"));

    let allowed = Permissions::sandboxed().allow_python(true);
    assert!(allowed.check_python().is_ok());
}

#[test]
fn test_permissions_sandbox_blocks_spawn() {
    let code = r#"
        spawn(function() { return 42; });
    "#;

    let program = Compiler::compile_source(code).expect("compile error");
    let mut vm = Vm::new(program).with_permissions(Permissions::sandboxed());
    vm.run();
    let err = vm.take_error().expect("Expected error when spawning in sandbox");
    assert!(
        err.to_string().contains("PermissionDenied"),
        "Expected PermissionDenied error, got: {}",
        err
    );
}

