use alloy_vm::compiler::Compiler;
use alloy_vm::vm::Vm;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn run_src(src: &str) -> (Vm, Arc<std::sync::Mutex<Vec<String>>>) {
    let program = Compiler::compile_source(src).expect("compile failed");
    let (mut vm, out) = Vm::with_output(program);
    vm.run();
    if let Some(err) = vm.take_error() {
        panic!("VM run error: {:?}", err);
    }
    (vm, out)
}

fn run_output(src: &str) -> String {
    let (_vm, sink) = run_src(src);
    let s = sink.lock().unwrap().join("\n");
    s
}

// ---------------------------------------------------------------------------
// 1. Stress: Actor Concurrency & Channel Message Throughput
// ---------------------------------------------------------------------------
#[test]
fn test_stress_actors_and_channels() {
    // 8 workers concurrently receiving batches of messages from multiple producers
    let src = r#"
        function sleep(ms) {
            const w = Promise.withResolvers();
            setTimeout(function () { w.resolve(1); }, ms);
            return w.promise;
        }

        async function main() {
            const NUM_WORKERS = 8;
            const MSGS_PER_WORKER = 500;
            const workerPromises = [];

            for (let w = 0; w < NUM_WORKERS; w++) {
                const chName = "stress_ch_" + w;
                channel.create(chName);

                // Spawn worker actor
                const p = spawn(function () {
                    const c = channel.get(chName);
                    return (async function () {
                        let sum = 0;
                        for (let i = 0; i < MSGS_PER_WORKER; i++) {
                            const val = await c.recv();
                            sum += val;
                        }
                        return sum;
                    })();
                });
                workerPromises.push(p);
            }

            // Producers sending messages to workers
            for (let i = 0; i < MSGS_PER_WORKER; i++) {
                for (let w = 0; w < NUM_WORKERS; w++) {
                    const ch = channel.get("stress_ch_" + w);
                    ch.send(1);
                }
            }

            let total = 0;
            for (let w = 0; w < NUM_WORKERS; w++) {
                const res = await workerPromises[w];
                total += res;
            }

            print("ACTOR_SUM:" + total);
        }
        main();
    "#;

    let out = run_output(src);
    assert_eq!(out, "ACTOR_SUM:4000");
}

// ---------------------------------------------------------------------------
// 2. Stress: 1,000,000+ Heap Allocations & GC Memory Stability
// ---------------------------------------------------------------------------
#[test]
fn test_stress_heap_and_gc() {
    let src = r#"
        function runAllocationStress() {
            let totalCreated = 0;
            // Loop 40 times, each allocating 10,000 objects, arrays, and strings (1,200,000 total)
            for (let batch = 0; batch < 40; batch++) {
                let list = [];
                for (let i = 0; i < 10000; i++) {
                    // Object with fields
                    const obj = { id: i, name: "item_" + i, nested: { val: i * 2 } };
                    // Cyclic reference
                    const a = { x: 1 };
                    const b = { y: 2 };
                    a.b = b;
                    b.a = a;
                    // Array of numbers and strings
                    const arr = [i, "str_" + i, true, null];
                    if (i % 1000 === 0) {
                        list.push(obj);
                    }
                    totalCreated += 3;
                }
            }
            return totalCreated;
        }

        const count = runAllocationStress();
        print("ALLOC_COUNT:" + count);
    "#;

    let start = Instant::now();
    let out = run_output(src);
    let elapsed = start.elapsed();
    assert_eq!(out, "ALLOC_COUNT:1200000");
    println!("Allocated 1,200,000 objects/arrays/strings in {:?}", elapsed);
}

// ---------------------------------------------------------------------------
// 3. Stress: Concurrent HTTP Server Requests
// ---------------------------------------------------------------------------
#[test]
fn test_stress_http_server_concurrent() {
    let (listener, port) = alloy_vm::vm::fuzz::bind_server(0).expect("bind");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    // Spawn server thread
    let server_handle = thread::spawn(move || {
        let src = r#"
            function handle(req, res) {
                if (req.method === "POST") {
                    res.status(200).json({ echo: req.body, path: req.path });
                } else {
                    res.status(200).text("PONG:" + req.path);
                }
            }
        "#;
        let program = Compiler::compile_source_with_mode(src, true, false).expect("compile");
        let mut vm = Vm::new(program);
        vm.run();
        let handler = vm.get_global("handle").expect("handle fn");

        alloy_vm::vm::fuzz::serve_loop(&mut vm, &handler, listener, &stop_clone);
    });

    thread::sleep(std::time::Duration::from_millis(50));

    // Fire 20 client threads, each making 25 requests (total 500 requests)
    let num_clients = 20;
    let reqs_per_client = 25;
    let mut client_handles = Vec::new();

    for client_id in 0..num_clients {
        let handle = thread::spawn(move || {
            let mut successful = 0;
            for r in 0..reqs_per_client {
                let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
                    Ok(s) => s,
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(10));
                        TcpStream::connect(("127.0.0.1", port)).expect("reconnect")
                    }
                };

                let req = if r % 2 == 0 {
                    format!("GET /test_{}_{} HTTP/1.1\r\nHost: localhost\r\n\r\n", client_id, r)
                } else {
                    let body = format!("{{\"client\":{},\"seq\":{}}}", client_id, r);
                    format!(
                        "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };

                stream.write_all(req.as_bytes()).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }

                let resp = String::from_utf8_lossy(&buf);
                if resp.contains("200 OK") {
                    successful += 1;
                }
            }
            successful
        });
        client_handles.push(handle);
    }

    let mut total_success = 0;
    for h in client_handles {
        total_success += h.join().expect("join client");
    }

    // Stop server
    stop.store(true, Ordering::SeqCst);
    // Wake listener by connecting one dummy socket
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = server_handle.join();

    assert_eq!(total_success, num_clients * reqs_per_client);
}

// ---------------------------------------------------------------------------
// 4. Stress: Hot Loop JIT Tiering & Numeric Compute
// ---------------------------------------------------------------------------
#[test]
fn test_stress_jit_and_hot_loops() {
    let src = r#"
        // Sieve of Eratosthenes up to 50,000
        function sieve(max) {
            const flags = [];
            for (let i = 0; i <= max; i++) {
                flags.push(true);
            }
            flags[0] = false;
            flags[1] = false;

            for (let p = 2; p * p <= max; p++) {
                if (flags[p]) {
                    for (let i = p * p; i <= max; i += p) {
                        flags[i] = false;
                    }
                }
            }

            let primeCount = 0;
            for (let i = 0; i <= max; i++) {
                if (flags[i]) {
                    primeCount++;
                }
            }
            return primeCount;
        }

        // Iterative Fibonacci with 100,000 loop passes
        function fibPasses(n) {
            let sum = 0;
            for (let iter = 0; iter < 100000; iter++) {
                let a = 0, b = 1;
                for (let i = 0; i < 20; i++) {
                    const tmp = a + b;
                    a = b;
                    b = tmp;
                }
                sum = a;
            }
            return sum;
        }

        const primes = sieve(50000);
        const fib = fibPasses(20);
        print("PRIMES:" + primes + " FIB:" + fib);
    "#;

    let start = Instant::now();
    let out = run_output(src);
    let elapsed = start.elapsed();
    assert_eq!(out, "PRIMES:5133 FIB:6765");
    println!("Sieve (50,000) and 100,000 Fib passes completed in {:?}", elapsed);
}
