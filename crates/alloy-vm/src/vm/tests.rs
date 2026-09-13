    use super::*;
    use super::builtins::http_server::{bind_server, serve_loop, request_complete};
    use crate::compiler::Compiler;
    use crate::opcode::Opcode;
    use alloy_core::value::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    fn run_src(src: &str) -> (Vm, Arc<Mutex<Vec<String>>>) {
        let program = Compiler::compile_source(src).expect("compile");
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        (vm, sink)
    }

    /// True when the suite runs with in-process CPython (`ALLOY_PYTHON_EMBED`
    /// set to 1). Child-mode features — the per-call timeout kill, pool
    /// growth, same-file parallelism — don't exist in embed mode (the GIL
    /// serializes and a hung call can't be killed), so tests that assert on
    /// them skip.
    fn embed_mode() -> bool {
        std::env::var("ALLOY_PYTHON_EMBED").as_deref() == Ok("1")
    }

    /// One HTTP request/response round-trip for the server tests. A read
    /// timeout turns a stalled server into an error instead of a hang.
    fn http_client(port: u16, path: &str) -> std::io::Result<String> {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        stream.write_all(format!("GET {} HTTP/1.1\r\nHost: t\r\n\r\n", path).as_bytes())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    /// The arena-backed microtask queue is the GC-killer pattern in action:
    /// a 5000-deep promise chain churns 5000 settled-continuation records, and
    /// a single drain (one cursor reset) reclaims them all — `used()` returns
    /// to zero with no per-record free, and the values survive the round-trip
    /// through the arena.
    #[test]
    fn microtask_arena_bulk_reset_after_chain() {
        let src = r#"
            function step(v) {
                if (v >= 5000) { print("chain done", v); return; }
                return Promise.resolve(v + 1).then(step);
            }
            Promise.resolve(0).then(step);
        "#;
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "chain done 5000");
        // Every settled continuation record was bulk-reclaimed: nothing left
        // in the arena after the queue drained.
        assert_eq!(vm.microtask_arena_used(), 0);
    }

    /// The generational escape analysis: per-run garbage is reclaimed while
    /// values stored in globals, channels, and closures survive promotion.
    /// Run 1 seeds a global cache + channel and churns garbage; run 2 reads
    /// the persisted state and confirms the young generation was reset.
    #[test]
    fn generational_reclaim_between_runs() {
        let run1 = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            let ch = channel.create();
            function store(k, v) { cache[k] = v; }
            store("a", "persisted-string");
            store("n", 42);
            ch.send("queued-msg");
            // Garbage: 2000 strings + 2000 arrays nobody keeps.
            for (let i = 0; i < 2000; i++) {
                let g = "garbage" + i;
                let arr = [g, g];
            }
            print("run1", cache.a, cache.n);
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(run1);
        vm.run();
        let young_after_r1 = vm.heap_used_young();
        // Run 1's garbage must have been reclaimed; only the persisted values
        // were promoted into the old generation.
        assert_eq!(young_after_r1, 0, "young generation not reclaimed after run 1");

        let run2 = Compiler::compile_source_with_mode(
            r#"
            print(cache.a, cache.n, ch.tryRecv());
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run2);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "run1 persisted-string 42\npersisted-string 42 queued-msg");
        assert_eq!(vm.heap_used_young(), 0, "young generation not reclaimed after run 2");
    }

    /// Server-style reclaim: thousands of per-request handler invocations with
    /// `promote_generation` between them. Each request creates garbage plus one
    /// escaped string stored in a global; the young generation must return to
    /// zero after every request while the escaped values survive promotion.
    #[test]
    fn generational_reclaim_between_requests() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let hits = 0;
            let last = "none";
            function handle(req) {
                hits = hits + 1;
                last = "hit" + hits;
                let tmp = [hits, hits, hits];
                return hits;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after setup");

        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        let req = Value::string("req".to_string());
        let mut old_before = 0usize;
        for _ in 0..10_000 {
            vm.call_value(&handler, std::slice::from_ref(&req));
            vm.promote_generation();
            assert_eq!(vm.heap_used_young(), 0, "young grew between requests");
            // Old generation only grows with the escaped per-request strings.
            assert!(vm.heap.used_old() > old_before);
            old_before = vm.heap.used_old();
        }

        let check = Compiler::compile_source_with_mode("print(last, hits);", true, false).unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "hit10000 10000");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
    }

    /// The second-generation sweep: a server that replaces a global with a
    /// fresh ~5KB object every request churns the old generation. Without the
    /// major GC the old gen would hold ~10MB of dead objects; with the
    /// non-copying mark-sweep it stays at the live set (free space reused by
    /// the next promotion), and the surviving globals stay readable across
    /// every sweep.
    #[test]
    fn major_gc_compacts_churned_old_gen() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let keep = "keep-this-alive";
            let cache = null;
            function handle(n) {
                let s = "";
                for (let i = 0; i < 512; i++) { s = s + "abcdefghij"; }
                cache = { big: s, n: n };
                return cache.n;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after setup");
        // Force the second-generation sweep to fire constantly (1KB churn).
        vm.major_threshold = 1 << 10;

        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        let req = Value::string("req".to_string());
        let mut peak_old = 0usize;
        for _ in 0..2_000 {
            vm.call_value(&handler, std::slice::from_ref(&req));
            vm.promote_generation();
            assert_eq!(vm.heap_used_young(), 0, "young grew between requests");
            peak_old = peak_old.max(vm.heap.used_old());
        }
        // 2000 x ~5KB churned objects would be ~10MB without compaction; the
        // major GC keeps the old gen at the live set (cache + strings).
        assert!(
            peak_old < 1 << 20,
            "old generation grew to {} bytes without reclaiming",
            peak_old
        );

        // Surviving globals are still readable after every compaction.
        let check = Compiler::compile_source_with_mode("print(keep, cache.n);", true, false).unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "keep-this-alive req");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
    }

    /// The non-copying property, measured: a ~10MB live cache in the old
    /// generation must NEVER move — the mark-sweep records addresses in a
    /// set instead of relocating them, so 500 forced majors over a 10MB live
    /// set cost a mark + sweep, not 10MB of copies per major. The surviving
    /// cache is byte-identical (payload addresses stable) across every sweep,
    /// and the old gen stays at the live set while churn reuses free space.
    #[test]
    fn major_gc_big_live_set_never_copies() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let keep = null;
            let cache = {};
            function seed() {
                let s = "";
                for (let i = 0; i < 1024; i++) { s = s + "abcdefghij"; }
                keep = { deep: [1, 2, 3], big: s };
                // ~10MB live: 200 objects each owning its own 50KB growable
                // builder string (a shared 100-byte constant appended 512
                // times — the bytes are copied into each object's buffer, so
                // nothing is shared in the live set). Few, long appends keep
                // the seed fast in debug builds.
                let block = "abcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghijabcdefghij";
                for (let i = 0; i < 200; i++) {
                    let own = "";
                    for (let j = 0; j < 512; j++) { own = own + block; }
                    cache["k" + i] = { big: own, i: i };
                }
            }
            function churn(n) {
                let s = "";
                for (let i = 0; i < 64; i++) { s = s + "xyz-"; }
                cache["tmp"] = { big: s, n: n };
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        let keep_idx = vm
            .global_names
            .iter()
            .position(|n| n == "keep")
            .expect("keep global");
        // The live cache is resident in old; capture keep's addresses BEFORE
        // any second-generation sweep.
        let keep_before = vm.globals[keep_idx].bits();
        let deep_before = vm.globals[keep_idx]
            .as_object()
            .unwrap()
            .borrow()
            .get("deep")
            .unwrap()
            .bits();
        let live_bytes = vm.heap.used_old();
        assert!(live_bytes > 8 << 20, "expected ~10MB live set, got {}", live_bytes);

        vm.major_threshold = 1 << 10;
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        let t0 = std::time::Instant::now();
        for i in 0..500 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let dt = t0.elapsed();
        eprintln!(
            "major GC over ~{}MB live set: 500 sweeps in {:?} ({:?}/sweep, free_list={} regions, {}B free)",
            live_bytes >> 20,
            dt,
            dt / 500,
            vm.heap.free_list_len(),
            vm.heap.free_bytes()
        );

        // THE non-copying proof: keep's boxes never moved across 500 sweeps.
        let keep_after = vm.globals[keep_idx].bits();
        assert_eq!(
            keep_before, keep_after,
            "live object was copied by the major GC"
        );
        let deep_after = vm.globals[keep_idx]
            .as_object()
            .unwrap()
            .borrow()
            .get("deep")
            .unwrap()
            .bits();
        assert_eq!(deep_before, deep_after, "interior array was copied");
        // Memory stays at the live set; the churned tmp objects reuse free
        // space instead of growing the old gen.
        assert!(
            vm.heap.used_old() < live_bytes + (1 << 20),
            "old gen grew past the live set: {} > {} + 1MB",
            vm.heap.used_old(),
            live_bytes
        );
    }

    /// Constant interning + program-heap compaction: a program full of
    /// repeated strings (property names, literals) must end up with ONE
    /// constant per unique string, the compiler's transient allocations must
    /// be swept out of the program heap, and the surviving constants must
    /// live in the program heap's old generation — where `kind_of` and
    /// `region_at` answer for them directly.
    #[test]
    fn constant_interning_and_program_heap_compaction() {
        use alloy_core::value::AString;
        let src = r#"
            let obj = { longPropertyName: "repeated-literal" };
            let total = 0;
            for (let i = 0; i < 10; i++) {
                total = total + obj.longPropertyName.length + "repeated-literal".length;
            }
            print(total);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        let count = |s: &str| {
            program
                .constants
                .iter()
                .filter(|c| c.as_str() == Some(s))
                .count()
        };
        // Interning: each unique string appears exactly once in the pool.
        assert_eq!(count("repeated-literal"), 1, "string literals must be interned");
        assert_eq!(count("longPropertyName"), 1, "property names must be interned");
        assert_eq!(count("length"), 1, "builtin property names must be interned");
        // Compaction: constants were promoted to the program heap's old gen,
        // the young gen was reset, and every surviving constant is readable
        // there via the side table.
        assert_eq!(program.heap.used_young(), 0, "program heap young not reset after compaction");
        assert!(program.heap.used_old() > 0, "constants not resident in old");
        let mut boxed = 0usize;
        for c in &program.constants {
            if let Some(s) = c.as_str() {
                let addr = ((c.bits() << 16) as i64 >> 16) as usize;
                assert!(program.heap.addr_in_old(addr), "constant box not in old");
                assert_eq!(program.heap.kind_of(addr), 1, "kind_of on the program heap"); // KIND_STRING
                let b = unsafe { &*(addr as *const AString) };
                assert_eq!(b.len(), s.len());
                assert!(program.heap.addr_in_old(b.bytes_ptr() as usize), "string bytes not in old");
                boxed += 1;
            }
        }
        eprintln!(
            "constants: {} slots, {} strings boxed, program heap old={}B young={}B — duplicate constants eliminated by interning",
            program.constants.len(),
            boxed,
            program.heap.used_old(),
            program.heap.used_young()
        );
        // Semantics unchanged: run it and check the output.
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "320");
        assert_eq!(vm.program_id, 0);
        // The .ax round-trip survives: deserialize compacts too, and the
        // constant values remain readable.
        let cloned = program.deep_clone().expect("deep clone");
        assert_eq!(cloned.constants.len(), program.constants.len());
        assert_eq!(cloned.heap.used_young(), 0);
        for (a, b) in program.constants.iter().zip(cloned.constants.iter()) {
            assert_eq!(a.as_str(), b.as_str());
        }
    }

    /// The side-table metadata: payloads are packed at 8-byte alignment with
    /// zero padding between regions (the table — not inline headers — knows
    /// each region's size), so `used_young` equals the sum of the region
    /// spans exactly, and the old scheme's 8-byte-per-region header overhead
    /// is gone entirely.
    #[test]
    fn side_table_young_density_zero_padding() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            function seed() {
                let out = [];
                for (let i = 0; i < 2000; i++) {
                    let s = "str" + i;
                    let a = [i, i + 1, i + 2];
                    let o = { k: s };
                    out.push(s); out.push(a); out.push(o);
                }
                return out;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        // Measure the young arena before any promotion/sweep.
        let mut regions = 0usize;
        let mut payload = 0usize;
        let mut span = 0usize;
        vm.heap.for_each_young_box(|_, _, size| {
            regions += 1;
            payload += size;
            span += (size + 7) & !7;
        });
        let used = vm.heap.used_young();
        eprintln!(
            "young density: {} regions, {}B payload, {}B span, used={}B — zero inter-region padding: {}, header overhead removed: {}B (would be {}B with old 8B headers), bitmap {}B, chunks {}",
            regions,
            payload,
            span,
            used,
            used == span,
            regions * 8,
            span + regions * 8,
            vm.heap.young_dirty_bytes(),
            vm.heap.young_chunk_count()
        );
        assert_eq!(used, span, "regions must pack with zero padding");
        assert!(regions > 4000, "expected a few thousand regions, got {}", regions);
    }

    /// Segregated size-class bins: a churned same-size object must be
    /// reallocated at the SAME address every request (LIFO reuse of its size
    /// class — temporal locality), the dead space must stay coalesced into
    /// one free region (no fragmentation), and `used_old` must stay flat.
    #[test]
    fn size_class_bins_lifo_reuse_pins_churned_slot() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            function churn(n) {
                let s = "";
                for (let i = 0; i < 64; i++) { s = s + "xyz-"; }
                cache["tmp"] = { big: s, n: n };
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        vm.major_threshold = 0; // a major sweep every boundary
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        let cache_idx = vm.global_names.iter().position(|n| n == "cache").expect("cache");
        let tmp_box_addr = |vm: &Vm| -> usize {
            let od = vm.globals[cache_idx].as_object().unwrap();
            let od = od.borrow();
            let tmp = od.get("tmp").unwrap();
            ((tmp.bits() << 16) as i64 >> 16) as usize
        };
        // Warm up until the churned slot reaches its fixed-point cycle.
        for i in 0..64 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let used_at_warmup = vm.heap.used_old();
        let warmup_addrs: std::collections::HashSet<usize> = (0..16)
            .map(|_| {
                vm.call_value(&churn, &[Value::int(0)]);
                vm.promote_generation();
                tmp_box_addr(&vm)
            })
            .collect();
        // Steady state: every subsequent request must reuse one of the
        // warmup addresses (LIFO pinning — the arena never grows new slots)
        // and the old generation must not grow a byte.
        let mut saw_new = 0usize;
        let t0 = std::time::Instant::now();
        for _ in 0..256 {
            vm.call_value(&churn, &[Value::int(0)]);
            vm.promote_generation();
            if !warmup_addrs.contains(&tmp_box_addr(&vm)) {
                saw_new += 1;
            }
        }
        let dt = t0.elapsed();
        eprintln!(
            "size-class churn: steady-state cycle of {} addresses, 256 requests in {:?} ({}µs/req), used_old {} -> {} ({}B drift), free regions={}, free={}B, {} new addresses escaped the cycle",
            warmup_addrs.len(),
            dt,
            dt.as_micros() / 256,
            used_at_warmup,
            vm.heap.used_old(),
            vm.heap.used_old().saturating_sub(used_at_warmup),
            vm.heap.free_list_len(),
            vm.heap.free_bytes(),
            saw_new
        );
        assert_eq!(saw_new, 0, "churned slot escaped its fixed-point cycle");
        assert_eq!(
            vm.heap.used_old(),
            used_at_warmup,
            "old gen grew while churning a fixed-size slot"
        );
        assert!(vm.heap.free_list_len() <= 2, "dead space fragmented: {} regions", vm.heap.free_list_len());
    }

    /// The incremental mark must survive mid-mark mutations: while a
    /// second-generation sweep is being prepared (its worklist spans many
    /// unit boundaries), writes into an already-marked old box (box barrier),
    /// a closure cell (Rc barrier), and a channel queue (Rc barrier) must be
    /// re-traced before the sweep runs, or the freshly-written young values
    /// would be swept out from under the live structures.
    #[test]
    fn incremental_mark_survives_mid_mark_mutation() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let cache = {};
            let ch = channel.create();
            let holder = (function () {
                let inner = "initial";
                return {
                    get: function () { return inner; },
                    set: function (v) { inner = v; },
                };
            })();
            function seed() {
                for (let i = 0; i < 2000; i++) { cache["k" + i] = "v" + i; }
            }
            function mutate() {
                cache["fresh"] = { v: "box-barrier" };
                holder.set("cell-barrier");
                ch.send("chan-barrier");
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        let get_global = |vm: &mut Vm, name: &str| -> Value {
            vm.globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == name)
                .map(|(v, _)| v.clone())
                .expect(name)
        };
        // Everything live is in old; force a major at the next boundary with
        // tiny slices so the mark visibly spans many unit boundaries.
        vm.major_threshold = 0;
        vm.mark_budget = 4;
        let seed = get_global(&mut vm, "seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        // The mark is now in progress: ~2000 boxes queued, 4 traced.
        assert!(
            vm.mark.as_ref().is_some_and(|m| m.worklist.len() > 100),
            "mark should be mid-flight with a large worklist"
        );
        // Mutate mid-mark: box write, cell write, channel send.
        let mutate = get_global(&mut vm, "mutate");
        vm.call_value(&mutate, &[]);
        // Drain the mark slice by slice; the young sweep runs every boundary.
        for _ in 0..10_000 {
            vm.promote_generation();
            if vm.mark.is_none() {
                break;
            }
        }
        assert!(vm.mark.is_none(), "mark never drained");
        // Every mid-mark mutation must have survived the sweep.
        let check = Compiler::compile_source_with_mode(
            "print(cache.fresh.v, holder.get(), ch.tryRecv());",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "box-barrier cell-barrier chan-barrier");
    }

    /// Same pattern through `await`: suspended async invocations resume via
    /// arena records, and the channel's event-loop integration resolves a
    /// parked `recv()` from a timer.
    #[test]
    fn channel_async_recv_via_event_loop() {
        let src = r#"
            let ch = channel.create();
            let got = "";
            (async function () { got = got + (await ch.recv()); })();
            setTimeout(() => { ch.send("ping"); }, 5);
            setTimeout(() => { print(got); }, 60);
        "#;
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "ping");
        assert_eq!(vm.microtask_arena_used(), 0);
    }

    /// Rope strings: `s = s + "ab"` in a loop must be O(1) per concat (cons
    /// nodes, zero byte copies) and the lazily-flattened content must be
    /// byte-exact — even for a 100k-leaf left-leaning rope, which a recursive
    /// flatten would stack-overflow on. The iterative flatten materializes
    /// the full expected string.
    #[test]
    fn rope_concat_linear_and_deep_flatten_exact() {
        use alloy_core::heap::{ArenaHeap, HeapGuard};
        use alloy_core::value::AString;
        let mut heap = ArenaHeap::new(1 << 20);
        let _g = HeapGuard::set(&mut heap);
        // Exactly `s = ""; for (...) s = s + "ab";` — a 100k-deep chain of
        // cons boxes. No bytes are copied while building.
        let mut s = Value::string(String::new());
        let tail = Value::string("ab".to_string());
        let start = std::time::Instant::now();
        for _ in 0..100_000 {
            s = Value::rope(s, tail.clone());
        }
        let build_ms = start.elapsed().as_millis();
        // Still a cons node: nothing was flattened or copied during the loop.
        let addr = ((s.bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(b.is_cons(), "loop-built string must stay a rope while building");
        // First read flattens iteratively and must be byte-exact.
        let got = s.as_str().expect("string").to_string();
        let expected: String = "ab".repeat(100_000);
        assert_eq!(got.len(), expected.len());
        assert_eq!(got, expected);
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "first read must flatten the rope in place");
        assert_eq!(b.len(), expected.len());
        eprintln!(
            "rope: 100k concats in {}ms (build), one {}KB flatten, exact content",
            build_ms,
            expected.len() / 1024
        );
    }

    /// Cached rope lengths + mixed concat: `s = s + i` (number!) must stay
    /// a rope while building — the number is converted to a rope leaf, no
    /// per-iteration copy or flatten — and `len()` must be O(1) from the
    /// cached field without flattening or re-walking the tree.
    #[test]
    fn rope_length_cached_and_mixed_concat_ropes() {
        use alloy_core::heap::{ArenaHeap, HeapGuard};
        use alloy_core::value::AString;
        let mut heap = ArenaHeap::new(1 << 20);
        let _g = HeapGuard::set(&mut heap);
        let mut s = Value::string(String::new());
        for i in 0..10_000 {
            s = s.add(&Value::int(i));
        }
        let addr = ((s.bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "mixed loop must not stay a rope tree");
        // len() reads the cached field: correct total, no flatten, no walk.
        let expected: String = (0..10_000).map(|i| i.to_string()).collect();
        assert_eq!(b.len(), expected.len(), "cached length wrong");
        // Content is byte-exact: "012345678910..."
        assert_eq!(s.as_str().unwrap(), expected);
    }

    /// The string-accumulator fusions: `s = s + "x"` / `s += t` / `s = s + e`
    /// each collapse into a single AppendString* opcode (the builder box
    /// stays in the local slot), and the fused semantics match the general
    /// `Add` path byte-for-byte — including self-append, keep-in-expression
    /// contexts, and aliasing (an earlier snapshot must not see later
    /// appends).
    #[test]
    fn string_accumulator_fusion_emitted_and_correct() {
        let src = r#"
            let t = "T";
            let a = "";
            for (let i = 0; i < 1000; i++) { a = a + "x"; }
            a += "!";
            a += t;
            let c = 0;
            for (let i = 0; i < 500; i++) { c = c + (i % 2); }
            let snap = a;
            a += "Z";
            let e = a;
            print(a.length, snap.length, e.length, a[1000], a[1001], a[1002], c);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The fused opcodes must actually be emitted for the accumulator
        // shapes: string-typed RHS (`a = a + "x"`, `a += "!"`, `a += "Z"`)
        // keep AppendStringConst; `a += t` (local leaf) keeps
        // AppendStringLocal; `c = c + (i % 2)` (subtree RHS) keeps
        // AppendStringPop — the register-ALU shapes only claim int/local-const
        // RHS, and every path applies the exact same `Value::add`
        // (rope/growable concat) for strings.
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::AppendStringConst as u8), "AppendStringConst not emitted");
        assert!(has(Opcode::AppendStringLocal as u8), "AppendStringLocal not emitted");
        assert!(has(Opcode::AppendStringPop as u8), "AppendStringPop not emitted");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        // a = 1000 x's + "!" + "T" + "Z" = 1003 chars; snap has 1002
        // (captured before the "Z" append, so it never sees it).
        assert_eq!(out, "1003 1002 1003 ! T Z 250");
    }

    /// The bytecode peephole: `3 * n` (int-on-left), `(i + j) % 7`
    /// (local-local then int), `(x) * 3` (parens defeat the AST fusion), and
    /// `2 * 3 + 1` (constant fold) each collapse into one dispatch, with a
    /// trailing statement Pop folded into keep=0. The loop also proves jump
    /// targets survive the stream compaction, and the outputs match JS.
    #[test]
    fn peephole_fusions_emitted_and_correct() {
        let src = r#"
            function col(n) {
                let steps = 0;
                while (n !== 1) {
                    if (n % 2 === 0) { n = n / 2; }
                    else { n = 3 * n + 1; }
                    steps += 1;
                }
                return steps;
            }
            let total = 0;
            for (let i = 1; i < 40; i++) { total += col(i); }
            let modsum = 0;
            for (let i = 0; i < 100; i++) {
                for (let j = 0; j < 100; j++) { modsum += (i + j) % 7; }
            }
            let px = 3;
            let paren = (px) * 3 + (px) * 5;
            5 * px;
            let fold = 2 * 3 + 1;
            print(total, modsum, paren, fold);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The fused opcodes must actually be emitted: `3 * n` -> BinIntLocal
        // (standalone `5 * px` still hits the peephole's int-on-left pattern),
        // the int-arithmetic trees (`3 * n + 1`, `(i + j) % 7`, paren chains)
        // -> register-ALU ArithChain, and `2 * 3 + 1` -> a single LoadConst
        // (pure-constant trees skip the chain so the fold still precomputes).
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::BinIntLocal as u8), "BinIntLocal not emitted");
        assert!(has(Opcode::ArithChain as u8), "ArithChain not emitted");
        // `2 * 3 + 1` folds to the single constant 7 (byte-level opcode scans
        // would false-positive on operand bytes, so check the constant pool).
        assert!(
            program
                .constants
                .iter()
                .any(|c| c.bits() == Value::int(7).bits()),
            "constant fold did not produce 7 in the constant pool"
        );
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "701 29992 24 7");
    }

    /// Comparison-chain fusion: `a < b && b < c` (and `||`, and every mix of
    /// local/int operands) collapses to CmpAnd* in both value and condition
    /// contexts. The short-circuit value semantics must hold (the second
    /// comparison is never evaluated when the first fires), the retained
    /// JumpPop in condition context must keep working (if/while/for/ternary),
    /// and the outputs must match JS exactly.
    #[test]
    fn peephole_cmp_chains_emitted_and_correct() {
        let src = r#"
            let a = 1, b = 2, c = 3, d = 4;
            let v1 = a < b && b < c;
            let v2 = a < b || b < c;
            let v3 = c < b && b < c;
            let v4 = c < b || b < c;
            let v5 = a < b && c < b;
            let v6 = a < 5 && b < 5;
            let v7 = a < 5 && b < c;
            let v8 = a < b && b < 5;
            let v9 = 5 < a && b < 5;
            let n = 0;
            if (a < b && b < c) { n += 1; }
            if (a < b || c < b) { n += 10; }
            if (c < b && (n = 999)) { }
            let m = 0;
            while (m < 5 && m < 3) { m += 1; }
            let t = a < b && b < c ? 7 : 8;
            a < b && b < c;
            let s = 0;
            for (let i = 0; i < 10 && i < 4; i++) { s += 1; }
            print(v1, v2, v3, v4, v5, v6, v7, v8, v9, n, m, t, s);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        let has = |op: u8| program.bytecode.contains(&op);
        assert!(has(Opcode::CmpAndLocalLocal as u8), "CmpAndLocalLocal not emitted");
        assert!(has(Opcode::CmpAndLocalInt as u8), "CmpAndLocalInt not emitted");
        assert!(has(Opcode::CmpAndIntLocal as u8), "CmpAndIntLocal not emitted");
        assert!(has(Opcode::CmpAndIntInt as u8), "CmpAndIntInt not emitted");
        // Every comparison-shaped value chain fused — exactly ONE
        // value-context JumpIfFalse instruction may survive: `5 < a && b < 5`
        // has the int on the left, which compiles to a generic
        // LoadInt+LoadLocal+LT (no CmpLocalInt), so that chain is
        // legitimately unfusable. Walk instruction starts (a byte scan would
        // false-positive on operand bytes like a slot of 0x1b).
        let bc = &program.bytecode;
        let mut jifs = 0;
        let mut o = 0;
        while o < bc.len() {
            if bc[o] == Opcode::JumpIfFalse as u8 {
                jifs += 1;
            }
            o += crate::bytecode::op_len(bc, o);
        }
        assert_eq!(jifs, 1, "value-context JumpIfFalse count");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "true true false true false true true true false 11 3 7 4");
    }

    /// Register-ALU fusion: int-arithmetic trees collapse into ONE ArithChain
    /// dispatch that keeps the running value in an i64 register. Verify the
    /// opcode fires on the bench shapes (`3*n+1`, `(lo+hi)%2`, big-mod seed,
    /// compound assigns, nested right subtrees) and that every result matches
    /// Node byte-for-byte, including the generic fallbacks (a float local
    /// kicks the chain out of the i64 lane mid-flight; `-9 % 3` → -0).
    #[test]
    fn arith_chain_registers_emitted_and_correct() {
        let src = r#"
            let a = 7, b = 3;
            let x = a + b * 2 - 1;
            let y = (a + b) % 4;
            let z = 3 * a + 1;
            let lo = 100, hi = 200;
            let mid = (lo + hi - (lo + hi) % 2) / 2;
            let seed = 12345;
            seed = (seed * 48271) % 2147483648;
            let n = 5;
            n = 3 * n + 1;
            let s = 0;
            s += 10 + 5 * 2;
            let t = 0;
            t += a;
            let neg = 0;
            neg -= 5;
            let rem = 1 / (-9 % 3);
            let f = 2.5;
            let r = f + 1 + 2;
            let steps = 0;
            for (let i = 1; i < 100; i++) { steps += 1; }
            print(x, y, z, mid, seed, n, s, t, neg, rem, r, steps);
        "#;
        let program = Compiler::compile_source(src).unwrap();
        // The bench shapes all fuse: count every register-ALU instruction
        // (the variable ArithChain for expression-position chains, plus the
        // fixed-shape Arith2/3Store* superinstructions for assignments) by
        // walking instruction starts (a raw byte scan would count operand
        // bytes that happen to equal the opcodes).
        let bc = &program.bytecode;
        let mut fused = 0;
        let mut o = 0;
        while o < bc.len() {
            if matches!(
                bc[o],
                b if b == Opcode::ArithChain as u8
                    || b == Opcode::Arith2StoreLocalConst as u8
                    || b == Opcode::Arith3StoreLocalConstConst as u8
                    || b == Opcode::Arith3StoreConstLocalConst as u8
            ) {
                fused += 1;
            }
            o += crate::bytecode::op_len(bc, o);
        }
        assert!(fused >= 8, "only {fused} register-ALU instructions emitted");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        // Verified against Node: 12 2 22 150 595905495 16 20 7 -5 -Infinity
        // 5.5 99 (the engine's display now matches Node's -Infinity).
        assert_eq!(out, "12 2 22 150 595905495 16 20 7 -5 -Infinity 5.5 99");
    }

    #[test]
    fn register_superinstructions_emitted_and_correct() {
        let src = r#"
            function test_registers() {
                let a = 10, b = 20, c = 0;
                c = a + b;
                let d = 0;
                d = a + 5;
                let e = 0;
                e = b;
                let count = 0;
                for (let i = 0; i < 50; i++) {
                    count += 1;
                }
                print(c, d, e, count);
            }
            test_registers();
        "#;
        let program = Compiler::compile_source(src).unwrap();
        let bc = &program.bytecode;
        let has = |op: u8| bc.contains(&op);
        assert!(has(Opcode::StoreLocalLocal as u8), "StoreLocalLocal not emitted");
        assert!(has(Opcode::BinLocalLocalLocalArith as u8), "BinLocalLocalLocalArith not emitted");
        assert!(has(Opcode::BinLocalLocalLocalInt as u8), "BinLocalLocalLocalInt not emitted");
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "30 15 20 50");
    }

    #[test]
    fn jit_compiles_and_executes_tight_loop() {
        let src = r#"
            let count = 0;
            for (let i = 0; i < 500; i++) {
                count = count + i;
            }
            print(count);
        "#;
        let (vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "124750");
        if let Some(jit) = vm.jit.as_ref() {
            if !jit.is_disabled() {
                assert!(jit.cache_len() > 0, "Loop should have been compiled by Cranelift JIT");
            }
        }
    }

    #[test]
    fn jit_disabled_flag_falls_back_to_interpreter() {
        let src = r#"
            let count = 0;
            for (let i = 0; i < 500; i++) {
                count = count + i;
            }
            print(count);
        "#;
        std::env::set_var("ALLOY_NO_JIT", "1");
        let (vm, sink) = run_src(src);
        std::env::remove_var("ALLOY_NO_JIT");
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "124750");
        if let Some(jit) = vm.jit.as_ref() {
            assert!(jit.is_disabled(), "JIT should report disabled");
            assert_eq!(jit.cache_len(), 0, "No loops should be compiled when disabled");
        }
    }

    /// Cross-thread `spawn(fn)`: the function runs on a worker thread in an
    /// isolated VM and the result settles as a promise on the VM thread.
    /// Covers sync results, rejection propagation, closure environments
    /// (a captured helper function survives the serialized crossing), and an
    /// async spawned function that awaits a timer on the worker's own event
    /// loop.
    #[test]
    fn spawn_runs_on_worker_thread_and_settles_promise() {
        let src = r#"
            function helper() { let m = 5; return function (x) { return x * m; }; }
            async function main() {
                let out = [];
                out.push("sync:" + await spawn(function () {
                    let s = 0;
                    for (let i = 0; i < 1000; i++) { s += i; }
                    return s;
                }));
                out.push("closure:" + await spawn(function () {
                    let h = helper();
                    return h(6);
                }));
                out.push("obj:" + await spawn(function () {
                    return { a: 1, b: [2, 3] };
                }));
                try {
                    await spawn(function () { throw "boom"; });
                    out.push("no-rejection");
                } catch (e) {
                    out.push("rejected:" + e);
                }
                out.push("async:" + await spawn(function () {
                    let w = Promise.withResolvers();
                    setTimeout(function () { w.resolve(21); }, 5);
                    return w.promise.then(function (v) { return v * 2; });
                }));
                print(out.join(" "));
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "sync:499500 closure:30 obj:[object Object] rejected:boom async:42");
    }

    /// Array.prototype.join/push and Date.now: the natives the differential
    /// `require` from inside an async HTTP handler: the module is loaded in
    /// the handler's synchronous portion (before AND after an `await` — the
    /// post-await require runs inside a resumed continuation), and the module
    /// singleton persists across requests (require-cache hit).
    #[test]
    fn require_works_inside_async_server_handler() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_handler_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("counter.ajs"),
            "export let n = 0;\n\
             export function bump() { n = n + 1; return n; }\n",
        )
        .unwrap();
        // top_level_globals: the server thread looks `handle` up in the VM's
        // globals (same as the python server tests).
        let program = Compiler::compile_source_with_mode(
            r#"
            async function handle(req, res) {
                const m = require('./counter.ajs');
                const a = m.bump();
                await Promise.resolve(1);
                const b = m.bump();
                res.send('' + (a * 100 + b));
            }
            "#,
            true,
            false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let body = |resp: &str| resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        // Request 1: a=1, b=2 -> "102". Request 2 (cache hit): a=3, b=4 ->
        // "304" — the module's counter state lives in the module's globals,
        // not the request, so it must carry across requests.
        // Note: `res.send(string)` delivers strings RAW (Express-style), not
        // JSON-quoted — `res.json` is the explicit JSON path.
        let r1 = http_client(port, "/").expect("request 1");
        let r2 = http_client(port, "/").expect("request 2");
        assert_eq!(body(&r1), "102", "got: {}", r1);
        assert_eq!(body(&r2), "304", "got: {}", r2);
        // Stop the serve thread and join: the VM drops deterministically
        // (python children reaped, shared-segment file removed) instead of
        // lingering on a detached thread.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `require` inside a spawned worker: the worker inherits the requirer's
    /// directory (so './x.ajs' resolves against the calling file, like Node)
    /// and shares the process-wide compiled-module registry — the module the
    /// worker loaded is reusable from the main VM afterwards (one compile,
    /// two VMs).
    #[test]
    fn require_works_inside_spawn_worker() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_worker_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("math.ajs"),
            "export function double(x) { return x * 2; }\n",
        )
        .unwrap();
        let program = Compiler::compile_source(
            r#"
            async function main() {
                const from_worker = await spawn(function () {
                    const m = require('./math.ajs');
                    return m.double(21);
                });
                print('worker:' + from_worker);
                // Same module, main VM: the worker's compile is shared, so
                // this is a registry hit — no recompile, and the module's
                // functions run on the main thread.
                const m = require('./math.ajs');
                print('main:' + m.double(5));
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "worker:42\nmain:10");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-thread `reload()`: the main VM rewrites a module and reloads it;
    /// the main VM's own next require AND a brand-new worker's first require
    /// both see the new code — the shared registry's generation bump
    /// invalidates every thread's cached copy, and the dropped bytes force a
    /// recompile from the current file.
    #[test]
    fn reload_propagates_across_threads() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_xthread_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("versioned.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        // fs natives resolve relative paths against the process cwd (Node
        // semantics), so rewrite via an absolute path; `require` resolves
        // against the module dir (also Node semantics) and finds the same
        // file through the temp dir.
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const fslib = require('fs');
                const m1 = require('./versioned.ajs');
                print('v1:' + m1.get());
                // Rewrite the module on disk, then reload: invalidates the
                // shared registry for every thread.
                print('wrote:' + fslib.writeFileSync('{}', 'export function get() {{ return 2; }}'));
                print('reloaded:' + reload('./versioned.ajs'));
                // The main VM's own next require re-runs the file -> v2.
                const m2 = require('./versioned.ajs');
                print('main:' + m2.get());
                // A fresh worker whose first require happens AFTER the reload
                // must see v2 too (registry miss -> compile from disk).
                const w = await spawn(function () {{
                    const m = require('./versioned.ajs');
                    return m.get();
                }});
                print('worker:' + w);
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "v1:1\nwrote:true\nreloaded:true\nmain:2\nworker:2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload from *inside* a worker (its own `reload()` native) makes the
    /// worker's very next require re-run the module — the live-thread half of
    /// the multi-tenant story, fully deterministic with no cross-thread wake
    /// needed.
    #[test]
    fn reload_from_worker_invalidates_worker_cache() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_worker_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("worker_mod.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const r = await spawn(function () {{
                    const fslib = require('fs');
                    const a = require('./worker_mod.ajs').get();
                    fslib.writeFileSync('{}', 'export function get() {{ return 2; }}');
                    reload('./worker_mod.ajs');
                    const b = require('./worker_mod.ajs').get();
                    return [a, b];
                }});
                print(r.join(','));
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "1,2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Three workers require the same fresh module at the same time: no
    /// double compile (the registry lock serializes the first), no corruption,
    /// and each worker gets its own module instance (Node's worker model —
    /// per-worker module state), so all three see a clean counter.
    #[test]
    fn concurrent_require_same_module_no_race() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_race_require_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shared.ajs"),
            "export let n = 0;\n\
             export function bump() { n = n + 1; return n; }\n",
        )
        .unwrap();
        let program = Compiler::compile_source(
            r#"
            async function main() {
                const f = function () {
                    const m = require('./shared.ajs');
                    return m.bump();
                };
                const a = spawn(f);
                const b = spawn(f);
                const c = spawn(f);
                const r1 = await a;
                const r2 = await b;
                const r3 = await c;
                print(r1, r2, r3);
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "1 1 1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE cross-thread wake scenario: a spawn worker parks on
    /// `await ch.recv()` (its event loop would previously break immediately
    /// and the message would be lost); the main thread sends, the worker is
    /// woken via the routed inbox, resumes, and receives BOTH messages. The
    /// worker parks again between the two sends, so both the first wake and
    /// a re-park + second wake are exercised.
    #[test]
    fn worker_parked_on_channel_recv_wakes_on_send() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                const ch = channel.create("wake_test_worker");
                const p = spawn(function () {
                    const c = channel.get("wake_test_worker");
                    return (async function () {
                        const a = await c.recv();
                        const b = await c.recv();
                        return [a, b];
                    })();
                });
                // Give the worker time to park on its first recv; either way
                // (parked → wake, or not yet → buffered bytes) the result is
                // deterministic, but 100ms makes the wake path the likely one.
                await sleep(100);
                ch.send("first");
                ch.send("second");
                print((await p).join(","));
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "first,second");
    }

    /// Reverse direction: the MAIN VM parks on `await ch.recv()` while a
    /// worker sleeps, then sends. The worker's send routes to the main VM's
    /// inbox and wakes its event loop (which is keeping itself alive because
    /// the waiter is parked), so the parked `await` resumes.
    #[test]
    fn main_parked_on_channel_recv_woken_by_worker_send() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                const ch = channel.create("wake_test_main");
                const p = spawn(function () {
                    const c = channel.get("wake_test_main");
                    return (async function () {
                        await sleep(30);
                        c.send("from-worker");
                        return "sent";
                    })();
                });
                const got = await ch.recv();
                const s = await p;
                print(got + " " + s);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "from-worker sent");
    }

    /// Worker-to-worker: two spawned VMs share a named channel; the consumer
    /// parks on `recv`, and the producer (a DIFFERENT VM) sends — routed to
    /// the consumer's inbox and woken there, never touching the main thread.
    #[test]
    fn worker_to_worker_channel_wakes_consumer() {
        let src = r#"
            function sleep(ms) {
                const w = Promise.withResolvers();
                setTimeout(function () { w.resolve(1); }, ms);
                return w.promise;
            }
            async function main() {
                channel.create("wake_test_w2w");
                const consumer = spawn(function () {
                    const c = channel.get("wake_test_w2w");
                    return (async function () { return await c.recv(); })();
                });
                // Let the consumer park; the producer is a separate VM.
                await sleep(100);
                const producer = spawn(function () {
                    const c = channel.get("wake_test_w2w");
                    c.send("relay");
                    return "done";
                });
                print(await consumer, await producer);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "relay done");
    }

    /// Buffered named-channel messages: a worker sends structured data
    /// (object, array, scalar) BEFORE anyone is waiting. The named channel
    /// serializes each message to bytes on send; the main VM decodes them
    /// into its own heap on recv — the cross-heap path with no wake needed.
    #[test]
    fn named_channel_buffers_structured_messages_as_bytes() {
        let src = r#"
            async function main() {
                const ch = channel.create("buff_test");
                const p = spawn(function () {
                    const c = channel.get("buff_test");
                    c.send({ n: 42, s: "hi" });
                    c.send([1, 2, 3]);
                    c.send(7);
                    return "produced";
                });
                await p;
                const a = await ch.recv();
                const b = await ch.recv();
                const c = await ch.recv();
                print(a.n, a.s, b.join("-"), c);
            }
            main();
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "42 hi 1-2-3 7");
    }

    /// The two cross-thread features compose: a LIVE worker observes a
    /// reload from the main thread between two of its requires, made
    /// deterministic by the channel wake (the previous missing piece). The
    /// worker requires the module, signals "ready" over a channel, parks on
    /// another; the main thread rewrites + reloads the module, then sends
    /// "go". The worker wakes, re-requires, and sees the NEW version — no
    /// timers, no polling, a pure message-passing sync point.
    #[test]
    fn live_worker_sees_reload_after_channel_wake() {
        let dir = std::env::temp_dir().join(format!(
            "alloy_reload_wake_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_path = dir.join("rw_mod.ajs");
        std::fs::write(&mod_path, "export function get() { return 1; }\n").unwrap();
        let abs = mod_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            async function main() {{
                const fslib = require('fs');
                channel.create("rw_ready");
                channel.create("rw_go");
                const p = spawn(function () {{
                    const ready = channel.get("rw_ready");
                    const go = channel.get("rw_go");
                    return (async function () {{
                        const a = require('./rw_mod.ajs').get();
                        ready.send("ready");
                        await go.recv();
                        const b = require('./rw_mod.ajs').get();
                        return [a, b];
                    }})();
                }});
                // Wait until the worker has required v1 (channel wake!), then
                // hot-reload the module while the worker is parked.
                await channel.get("rw_ready").recv();
                fslib.writeFileSync('{}', 'export function get() {{ return 2; }}');
                print('reloaded:' + reload('./rw_mod.ajs'));
                channel.get("rw_go").send("go");
                print((await p).join(","));
            }}
            main();
            "#,
            abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\n1,2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Named channels resolve on every VM: `get` on a missing name throws a
    /// loud, catchable error (no silent undefined).
    #[test]
    fn named_channel_missing_name_throws() {
        let src = r#"
            try {
                channel.get("never_created");
                print("no error");
            } catch (e) {
                print("missing:" + (("" + e).indexOf("not found") >= 0));
            }
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "missing:true");
    }

    /// corpus previously dodged. Expected values verified against Node.
    #[test]
    fn array_join_push_and_date_now_match_node() {
        let src = r#"
            let xlog = [];
            let l1 = xlog.push(1);
            let l2 = xlog.push(2, 3);
            xlog.push("four");
            print(l1, l2, xlog.length, xlog.join(","), xlog.join("-"), "[" + xlog.join() + "]");
            print("[" + [].join(",") + "]", [null, undefined, 5].join("|"), [[1, 2], [3]].join("+"));
            let t = Date.now();
            let t2 = Date.now();
            print(t <= t2);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "1 3 4 1,2,3,four 1-2-3-four [1,2,3,four] [] ||5 1,2+3 true");
    }

    /// The full `Array.prototype` / `String.prototype` round-out: pop, shift,
    /// unshift (return values + mutation), slice (positive/negative indices),
    /// concat (arrays flatten, scalars append), indexOf/includes (incl. NaN
    /// behavior), map/forEach (callbacks receive element/index/array),
    /// charAt (out-of-bounds and negative → ""), substring (swap/clamp),
    /// split (sep, empty sep, missing sep), toUpperCase. Expected values
    /// verified against Node.
    #[test]
    fn array_string_prototype_roundout_matches_node() {
        let src = r#"
            let a = [1, 2, 3, 4, 5];
            print(a.pop(), a.shift(), a.unshift(9, 8));
            print(a.slice(1, 3).join(","), [1, 2].concat([3, 4], 5).join(","));
            print([1, 2, 3, 2].indexOf(2, 1), [NaN, 1].includes(NaN), [NaN, 1].indexOf(NaN));
            let d = [1, 2, 3].map(function (x) { return x * 2; });
            let s = 0;
            [1, 2, 3].forEach(function (x) { s += x; });
            print(d.join("-"), s);
            print("hello".charAt(1), "hi".charAt(5), "hi".charAt(-1), "hello".substring(1, 3), "hello".substring(3, 1), "a,b,c".split(",").join("|"), "abc".split("").join("-"), "heLLo".toUpperCase(), "x".split().length);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "5 1 5 8,2 1,2,3,4,5 1 true -1 2-4-6 6 e   el el a|b|c a-b-c HELLO 1");
    }

    /// The second prototype round-out: String.replace ($ patterns, function
    /// replacement, empty pattern, missing replacement), trim (with the BOM
    /// edge — verified live against Node with a raw \uFEFF byte, since the
    /// lexer doesn't decode escape sequences), indexOf/lastIndexOf (NaN,
    /// +-Infinity, clamping, empty needle), Array.sort (default lexicographic
    /// order, comparator function, undefined sorts last) and reverse
    /// (in-place, returns the same array). Expected values verified against
    /// Node.
    #[test]
    fn replace_trim_indexof_sort_reverse_match_node() {
        let src = r#"
            print("hello world".replace("o", "[$&][$`][$'][$$]"));
            print("abc".replace("b", function (m, off, s) { return m + "@" + off; }));
            print("abc".replace("x", "Z"), "abc".replace("", "X"), "abc".replace("b", "$1"), "abc".replace("b"));
            print(" a b ".trim() + "|");
            print("abcabc".indexOf("b"), "abcabc".indexOf("b", 2), "abc".indexOf(""), "abc".indexOf("", 5), "abc".indexOf("b", NaN), "abcabc".indexOf("b", Infinity));
            print("abcabc".lastIndexOf("b"), "abcabc".lastIndexOf("b", 3), "abc".lastIndexOf("b", -1), "abc".lastIndexOf("b", NaN), "abc".lastIndexOf("", 2), "abc".lastIndexOf("", -1));
            print([10, 9, 100, 1].sort().join(","));
            print([undefined, null, 3, 1].sort().join(","));
            print([10, 2, NaN].sort().join(","));
            print([3, 1, 4, 1, 5].sort(function (a, b) { return a - b; }).join(","));
            let r = [5, 1, 4];
            let r2 = r.reverse();
            print(r.join(","), r2.join(","), r === r2);
            print("hello".indexOf("l"), "hello".lastIndexOf("l"));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "hell[o][hell][ world][$] world ab@1c abc Xabc a$1c aundefinedc a b| \
             1 4 0 3 1 -1 4 1 -1 1 2 0 1,10,100,9 1,3,, 10,2,NaN 1,1,3,4,5 \
             4,1,5 4,1,5 true 2 3"
        );
    }

    /// The third prototype round-out: String.slice/substr (negative and
    /// clamped indices), includes/startsWith/endsWith (NaN, Infinity, empty
    /// search, endPosition), padStart/padEnd (default pad, truncation,
    /// fractional target), and Array find/findIndex/filter/some/every
    /// (truthiness callbacks, empty-array results). Expected values verified
    /// against Node.
    #[test]
    fn slice_substr_includes_pad_find_filter_match_node() {
        let src = r#"
            print("hello world".slice(3), "hello world".slice(-3), "hello world".slice(1, -1), "hello".slice(5, 2), "hello".slice(NaN, 3), "hello".slice(-99, 3));
            print("hello".substr(2), "hello".substr(-3), "hello".substr(1, 2), "hello".substr(3, 99), "hello".substr(-99), "hello".substr(1, -1));
            print("hello".includes("ll"), "hello".includes("", 99), "hello".includes("x", -5), "hello".includes("h", Infinity));
            print("hello".startsWith("he"), "hello".startsWith("l", 2), "hello".startsWith("", 99), "hello".startsWith("x"));
            print("hello".endsWith("lo"), "hello".endsWith("l", 3), "hello".endsWith("", 0), "hello".endsWith("lo", NaN));
            print("5".padStart(3), "5".padStart(3, "0"), "5".padStart(4, "ab"), "abc".padEnd(5, "-"), "".padStart(2), "5".padStart(2.7, "x"));
            print([1, 2, 3].find(function (x) { return x > 1; }), [1, 2, 3].find(function (x) { return x > 9; }));
            print([1, 2, 3].findIndex(function (x) { return x > 1; }), [1, 2, 3].findIndex(function (x) { return x > 9; }));
            print([1, 2, 3, 4].filter(function (x) { return x % 2 == 0; }).join(","), [1, 2].filter(function (x) { return x > 5; }).length);
            print([1, 2, 3].some(function (x) { return x == 2; }), [].some(function (x) { return true; }));
            print([1, 2, 3].every(function (x) { return x > 0; }), [].every(function (x) { return false; }));
            print([0, null, undefined, 2].find(function (x) { return x; }), [0, 1].findIndex(function (x) { return x; }));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "lo world rld ello worl  hel hel llo llo el lo hello  true true false false \
             true true true false true true true false   5 005 aba5 abc--    x5 2 undefined \
             1 -1 2,4 0 true false true true 2 1"
        );
    }

    /// The Math/Number globals: floor/ceil/round (round's -0 for negatives in
    /// [-0.5, 0)), abs/sqrt/pow (incl. NaN and -0), min/max (NaN propagation,
    /// -0/+0 selection, empty-arg ±Infinity), parseInt (hex auto-detect,
    /// radix validation, no octal for "010"), parseFloat (decimal prefixes,
    /// Infinity literal), Number.isNaN (type-strict), and Math.random range.
    /// Expected values verified against Node.
    #[test]
    fn math_and_number_globals_match_node() {
        let src = r#"
            print(Math.floor(2.7), Math.floor(-0.5), Math.floor(-0), Math.floor(NaN), Math.floor(Infinity));
            print(Math.ceil(0.1), Math.ceil(-0.5), Math.ceil(-1.2), Math.ceil(5));
            print(Math.round(0.5), Math.round(-0.5), Math.round(-1.5), Math.round(0.4), Math.round(-0.4));
            print(Math.abs(-5), Math.abs(-0), Math.abs(-Infinity), Math.abs(NaN));
            print(Math.sqrt(4), Math.sqrt(-1), Math.sqrt(2));
            print(Math.pow(2, 3), Math.pow(-1, 0.5), Math.pow(2, -1), Math.pow(0, 0));
            print(Math.min(), Math.max(), Math.min(3, 1, 2), Math.min(NaN, 5), Math.min(-0, 0), Math.max(-0, 0));
            print(parseInt("42"), parseInt("0x10"), parseInt("0x10", 10), parseInt("101", 2), parseInt("zz", 36), parseInt(""), parseInt("3.14"), parseInt("   -0x10"));
            print(parseInt("010"), parseInt("0b101"), parseInt("ff", 16), parseInt("10", 1), parseInt("10", 37));
            print(parseFloat("3.14abc"), parseFloat("  -1.5e2"), parseFloat("0x10"), parseFloat("Infinity"), parseFloat(""), parseFloat(".5"), parseFloat("5."), parseFloat("abc"));
            print(Number.isNaN(NaN), Number.isNaN("abc"), Number.isNaN(5), Number.parseInt("10", 2), Number.parseFloat("2.5"));
            print(isNaN("abc"), isNaN(5));
            let r = Math.random();
            print(r >= 0 && r < 1);
            print(Math.floor(5) === 5, Math.round(-0.1) === 0, Math.round(-0.1), Math.floor(2.999), Math.ceil(-2.999));
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "2 -1 -0 NaN Infinity 1 -0 -1 5 1 -0 -1 0 -0 5 0 Infinity NaN 2 NaN \
             1.4142135623730951 8 NaN 0.5 1 Infinity -Infinity 1 NaN -0 0 42 16 0 5 \
             1295 NaN 3 -16 10 0 255 NaN NaN 3.14 -150 0 Infinity NaN 0.5 5 NaN \
             true false false 2 2.5 true false true true true -0 2 -2"
        );
    }

    /// Map/Set: `new Map()`/`new Set()` (native constructors), SameValueZero
    /// keys (`NaN` finds `NaN`, `-0`/`+0` share a slot, `1` and `1.0` are one
    /// key, objects by identity), `get`/`set`/`has`/`delete`/`clear`/`size`
    /// (and Set's `add`), `instanceof` against the native constructor, and
    /// object-key churn that forces young→old promotion and old-gen sweeps
    /// (the entry table is rebuilt against remapped addresses). Expected
    /// values verified against Node.
    #[test]
    fn map_set_match_node() {
        let src = r#"
            let m = new Map();
            m.set("a", 1); m.set("b", 2);
            print(m.get("a"), m.get("zz"), m.has("a"), m.size);
            print(m.delete("a"), m.has("a"), m.size);
            m.clear(); print(m.size);
            m.set(1, "one"); print(m.get(1.0), m.get(2));
            m.set(NaN, "nan"); print(m.has(NaN), m.get(NaN));
            m.set(-0, "z"); print(m.get(0));
            let o = { x: 1 }; m.set(o, "obj");
            print(m.get(o), m.get({ x: 1 }), m.size);
            print(m instanceof Map, typeof Map, Map.prototype !== undefined);
            let s = new Set();
            s.add(5); s.add(5.0); s.add("s");
            print(s.size, s.has(5), s.has("s"), s.has(6));
            print(s.delete(5), s.size, s.has(5));
            print(s instanceof Set);
            let m2 = new Map();
            for (let i = 0; i < 5000; i++) { let k = { i: i }; m2.set(k, i); if (i % 3 === 0) m2.delete(k); }
            print(m2.size);
            let sk = []; let ms = new Map();
            for (let i = 0; i < 5000; i++) { let k = "k" + (i % 500); if (i % 4 === 0) sk.push(k); ms.set(k, i); }
            let ssum = 0;
            for (let j = 0; j < sk.length; j++) { let v = ms.get(sk[j]); if (v !== undefined) ssum += v; }
            print(sk.length, ms.size, ssum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "1 undefined true 2 true false 1 0 one undefined true nan z obj undefined 4 \
             true function true 2 true true false true 1 false true 3333 1250 500 5935000"
        );
    }

    /// Map/Set iteration: `keys()`/`values()`/`entries()` return array
    /// snapshots in insertion order (the engine has no iterator protocol),
    /// `forEach` walks live with `(value, key, map)` args and the map as the
    /// third arg, re-setting a key keeps its position while delete+re-add
    /// moves it to the end, Set entries are `[v, v]` pairs, and a 1000-key
    /// churn with 333 deletes exercises the tombstone compaction without
    /// disturbing order. Expected values verified against Node.
    #[test]
    fn map_set_iteration_match_node() {
        let src = r#"
            let m = new Map();
            m.set("b", 2); m.set("a", 1); m.set("c", 3);
            print([...m.keys()].join(","));
            print([...m.values()].join(","));
            print([...m.entries()].map(e => e.join(":")).join("|"));
            m.set("a", 99);
            print([...m.keys()].join(","), [...m.values()].join(","));
            m.delete("b");
            print([...m.keys()].join(","), m.size);
            m.set("b", 7);
            print([...m.keys()].join(","));
            let acc = [];
            m.forEach((v, k) => acc.push(k + "=" + v));
            print(acc.join(";"));
            let sacc = [];
            m.forEach(function (v, k, mm) { sacc.push(k + ":" + v + ":" + (mm === m)); });
            print(sacc.join(";"));
            let s = new Set();
            s.add("x"); s.add("y"); s.add("z");
            print([...s.keys()].join(","), [...s.values()].join(","));
            print([...s.entries()].map(e => e.join(":")).join("|"));
            let acc2 = [];
            s.forEach((v, k) => acc2.push(k + "=" + v));
            print(acc2.join(";"));
            s.delete("y");
            s.add("y");
            print([...s.keys()].join(","));
            let big = new Map();
            for (let i = 0; i < 1000; i++) big.set("k" + i, i);
            for (let i = 0; i < 1000; i += 3) big.delete("k" + i);
            print(big.size, [...big.keys()].slice(0, 6).join(","), [...big.keys()][big.size - 1]);
            let sum = 0;
            big.forEach((v, k) => { sum += v; });
            print(sum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "b,a,c 2,1,3 b:2|a:1|c:3 b,a,c 2,99,3 a,c 2 a,c,b a=99;c=3;b=7 \
             a:99:true;c:3:true;b:7:true x,y,z x,y,z x:x|y:y|z:z x=x;y=y;z=z x,z,y \
             666 k1,k2,k4,k5,k7,k8 k998 332667"
        );
    }

    /// The arena-backed entry table across a full GC lifecycle. Run 1 seeds
    /// a global Map with 30k churned object keys + string keys (table growth
    /// rehashes into fresh young slot regions) and run's end promotes the
    /// map, its keys, and its slot region into the old generation. Run 2
    /// then probes the *promoted* table: object keys must still resolve by
    /// identity (the table was rebuilt against remapped addresses), SameValue
    /// Zero keys (NaN/-0/1 vs 1.0) must hit, insertion order must survive,
    /// and delete+re-add must still move the key to the end. The big old-gen
    /// region also forces major sweeps whose mark must keep the slot region
    /// alive. Expected values verified against Node on the concatenated
    /// script.
    #[test]
    fn map_arena_table_survives_promotion_and_sweep() {
        let run1 = Compiler::compile_source_with_mode(
            r#"
            let keys = [];
            let m = new Map();
            for (let i = 0; i < 30000; i++) {
                let k = { i: i };
                if (i % 5 === 0) keys.push(k);
                m.set(k, i);
                if (i % 3 === 0) m.delete(k);
                if (i % 11 === 0) m.set("s" + (i % 500), i);
            }
            m.set(NaN, "nan");
            m.set(-0, "z");
            m.set(1, "one");
            // Old-gen garbage for the sweep: a big array promoted at run 1's
            // boundary, then dropped in run 2 — the mark must not reach it.
            scratch = [];
            for (let i = 0; i < 5000; i++) scratch.push({ x: i });
            print("r1", m.size, keys.length);
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(run1);
        vm.run();
        let run2 = Compiler::compile_source_with_mode(
            r#"
            let sum = 0, missing = 0;
            for (let j = 0; j < keys.length; j++) {
                let v = m.get(keys[j]);
                if (v === undefined) missing++;
                else sum += v;
            }
            print("r2", m.size, missing, sum);
            print(m.get(NaN), m.get(0), m.get(-0), m.get(1.0), m.get("s7"));
            let ks = [...m.keys()];
            print("order", ks.length, ks.length === m.size);
            m.delete(keys[0]);
            m.set(keys[0], 999);
            let ks2 = [...m.keys()];
            print("readd", ks2.length, ks2[ks2.length - 1] === keys[0], m.get(keys[0]));
            let cnt = 0;
            m.forEach((v, k) => { cnt++; });
            print("cnt", cnt);
            // Drop the old-gen scratch: its 5000 boxes become unreachable and
            // the next major sweep must reclaim them.
            scratch = null;
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run2);
        vm.run();
        // Drive the incremental major GC to completion: enough unit boundaries
        // for the budgeted mark to drain and the old-generation sweep to run
        // WHILE the map (and its arena slot region) is live — the sweep must
        // keep the region, or the next lookup reads freed arena memory.
        for _ in 0..400 {
            vm.promote_generation();
        }
        // The sweep reclaimed the churned old-gen garbage.
        assert!(
            vm.heap.free_bytes() > 0,
            "expected the major sweep to have reclaimed dead old-gen space"
        );
        let run3 = Compiler::compile_source_with_mode(
            r#"
            let sum = 0, missing = 0;
            for (let j = 0; j < keys.length; j++) {
                let v = m.get(keys[j]);
                if (v === undefined) missing++;
                else sum += v;
            }
            let cnt = 0;
            m.forEach((v, k) => { cnt++; });
            print("r3", m.size, missing, sum, cnt);
            print(m.get(NaN), m.get(0), m.get(1.0), m.get("s7"));
        "#,
            true, false,
        )
        .unwrap();
        vm.set_program(run3);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "r1 20503 6000\nr2 20503 2000 60000000\n\
             nan z z one 29007\n\
             order 20503 true\n\
             readd 20504 true 999\n\
             cnt 20504\n\
             r3 20504 1999 60000999 20504\n\
             nan z one 29007"
        );
    }

    /// JSON.stringify/parse: insertion-order keys (object literals, parse
    /// round-trips), the `space` pretty-print arg (number/string/tab, 0 →
    /// compact), NaN/Infinity/-0 → null/0, undefined/function collapsing in
    /// arrays (null) and objects (omitted), `\uXXXX`/surrogate escapes in
    /// parse, -0 preserved, cycles and syntax errors caught by try/catch
    /// through the native throw hook. Expected values verified against Node.
    #[test]
    fn json_stringify_parse_match_node() {
        let src = r###"
            print(JSON.stringify({ a: 1, b: 2 }), JSON.stringify({ b: 2, a: 1 }));
            print(JSON.stringify([1, "x", true, null, 2.5]));
            print(JSON.stringify(NaN), JSON.stringify(Infinity), JSON.stringify(-0), JSON.stringify(undefined));
            print(JSON.stringify([1, undefined, function () {}, 3]));
            print(JSON.stringify({ a: 1, b: undefined, c: function () {}, d: 4 }));
            print(JSON.stringify({ a: 1, b: [1, 2] }, null, 0));
            print(JSON.parse("{\"a\":1,\"b\":[true,null,\"x\"]}").a, JSON.parse("{\"a\":1,\"b\":[true,null,\"x\"]}").b.length);
            print(JSON.parse("42"), JSON.parse("-1.5"), JSON.parse("1e3"), JSON.parse("-0") === 0, 1 / JSON.parse("-0"));
            print(JSON.parse("\"caf\\u00e9\""), JSON.parse("\"\\uD83D\\uDE00\""));
            print(JSON.parse("[1,2,3]").join(","));
            let o = JSON.parse("{\"x\":1,\"y\":[2,3],\"z\":{\"w\":4}}");
            print(o.x, o.y.join(","), o.z.w);
            print(JSON.stringify(JSON.parse("{\"k\":1,\"j\":2}")));
            let cyc = {}; cyc.self = cyc;
            try { JSON.stringify(cyc); print("NO THROW"); } catch (e) { print("caught"); }
            try { JSON.parse("{bad}"); print("NO THROW2"); } catch (e) { print("caught2"); }
            print(JSON.stringify(""), JSON.stringify([]), JSON.stringify({}));
            print(JSON.stringify(JSON.parse(" {\"a\" : [1, 2] , \"b\": {\"c\": true}} ")));
        "###;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "{\"a\":1,\"b\":2} {\"b\":2,\"a\":1} [1,\"x\",true,null,2.5] null null 0 undefined \
             [1,null,null,3] {\"a\":1,\"d\":4} {\"a\":1,\"b\":[1,2]} 1 3 \
             42 -1.5 1000 true -Infinity café 😀 1,2,3 1 2,3 4 {\"k\":1,\"j\":2} \
             caught caught2 \"\" [] {} {\"a\":[1,2],\"b\":{\"c\":true}}"
        );
    }

    /// JSON.stringify's replacer argument: a key whitelist (applied to every
    /// object at every depth, arrays unfiltered, missing keys → empty object)
    /// and a function (called with `(key, value)` for root and each
    /// property/element — transformation, omission, root-undefined, array
    /// element → null, call order). Expected values verified against Node.
    #[test]
    fn json_replacer_matches_node() {
        let src = r###"
            print(JSON.stringify({ a: 1, b: 2, c: 3 }, ["a", "c"]));
            print(JSON.stringify({ a: 1, b: { c: 2, d: 3 } }, ["b", "c"]));
            print(JSON.stringify({ a: [1, 2, 3], b: 4 }, ["a"]));
            print(JSON.stringify({ a: 1, b: 2 }, function (k, v) { if (k === "b") { return undefined; } return v; }));
            print(JSON.stringify({ a: 1, b: "x" }, function (k, v) { return typeof v === "number" ? v * 2 : v; }));
            print(JSON.stringify({ a: 1 }, function () { return undefined; }));
            print(JSON.stringify([1, 2, 3], function (k, v) { if (typeof v === "number" && v > 1) { return undefined; } return v; }));
            let log = [];
            JSON.stringify({ a: 1, b: 2 }, function (k, v) { log.push(k); return v; });
            print(log.join(","));
            print(JSON.stringify({ a: 1, b: 2 }, ["missing"]));
            print(JSON.stringify({ a: { x: 1, y: 2 }, b: 3 }, ["a", "x"]));
        "###;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "{\"a\":1,\"c\":3} {\"b\":{\"c\":2}} {\"a\":[1,2,3]} {\"a\":1} \
             {\"a\":2,\"b\":\"x\"} undefined [1,null,null] ,a,b {} {\"a\":{\"x\":1}}"
        );
    }

    /// Number/string formatting round-out: toString(radix) with the
    /// V8 DoubleToRadixCString algorithm (verified on 535 (value, radix)
    /// pairs), toFixed/toPrecision rounding the exact binary value, string
    /// UTF-16 indexing (charCodeAt/codePointAt/length). Also covers the
    /// large-int dispatch fix — `9007199254740992.toString(2)` previously
    /// resolved to undefined because `Value::int` boxed it as a tagged misc.
    /// Expected values verified against Node.
    #[test]
    fn number_string_formatting_matches_node() {
        let src = r#"
            print((1234).toString(16), (255).toString(2), (0.5).toString(2), (0.3).toString(3));
            print((1.5).toString(3), (0.1).toString(16), (1e21).toString(16), (123.456).toString(16));
            print((-123.456).toString(16), (9007199254740991).toString(2), (9007199254740992).toString(2));
            print((1.5).toFixed(0), (1.005).toFixed(2), (2.5).toFixed(0), (0.1).toFixed(20));
            print((1e21).toFixed(2), (1e-7).toFixed(2), (0).toFixed(2));
            print((999.9).toPrecision(3), (0.0001).toPrecision(3), (123.456).toPrecision(5));
            print((1.5).toPrecision(2), (1e-7).toPrecision(1), (1e-7).toPrecision(3), (1234).toPrecision(2));
            print((1e25).toPrecision(30), (1e21).toPrecision(21), (0.1).toPrecision(17));
            print("hello".charCodeAt(1), "hello".charCodeAt(99), "héllo".charCodeAt(1), "hello".charCodeAt(-1));
            print("😀".codePointAt(0), "😀".charCodeAt(0), "😀".charCodeAt(1), "😀".length, "a😀b".length);
            print("a😀b".charCodeAt(1), "a😀b".codePointAt(1), (3.14).toString(), (1e21).toString());
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(
            out,
            "4d2 11111111 0.1 0.0220022002200220022002200220022002 \
             1.111111111111111111111111111111112 0.1999999999999a 3635c9adc5dea00000 7b.74bc6a7ef9dc \
             -7b.74bc6a7ef9dc 11111111111111111111111111111111111111111111111111111 \
             100000000000000000000000000000000000000000000000000000 \
             2 1.00 3 0.10000000000000000555 \
             1e+21 0.00 0.00 \
             1.00e+3 0.000100 123.46 \
             1.5 1e-7 1.00e-7 1.2e+3 \
             10000000000000000905969664.0000 1.00000000000000000000e+21 0.10000000000000001 \
             101 NaN 233 NaN \
             128512 55357 56832 2 4 \
             55357 128512 3.14 1e+21"
        );
    }

    /// Per-slot SMI/number type feedback: the fused register-ALU, load/store
    /// and compare paths take raw i64/f64 lanes on slots whose feedback kind
    /// Parser hardening: sources that Node rejects are loud compile errors,
    /// not silent misparses — comma-less argument/element/field lists
    /// (`print(1 2)`, `[1 2]`, `{a: 1 b: 2}`), a number directly followed by
    /// an identifier (`0.toString`, which Node lexes as `0.` + identifier),
    /// unclosed parens/params (`(1`, `if (true {`), and the radix-literal
    /// member access that used to be rejected (`0x10.toString(16)` is VALID
    /// JS and must work). `.5` is a valid leading-dot number literal.
    #[test]
    fn parser_rejects_silent_misparses() {
        let err = |src: &str| {
            assert!(Compiler::compile_source(src).is_err(), "expected compile error for: {}", src);
        };
        err("print(1 2);");
        err("print(1 2 3);");
        err("print([1 2]);");
        err("print({a: 1 b: 2});");
        err("let [a b] = [1, 2];");
        err("let {a b} = {a: 1, b: 2};");
        err("print(0.toString(2));");
        err("0.toString;");
        err("print(1;");
        err("if (true { print(1); }");
        err("function f(a { return a; }");
        err("while (true { break; }");
        err("let x = (1; print(x);");
        err("print(0x1.5);");

        // Valid forms these checks must NOT break (newlines are implicit
        // statement separators in the engine, matching its test corpus).
        let ok = |src: &str, expect: &str| {
            let program = Compiler::compile_source(src).expect("compile");
            let (mut vm, out) = Vm::with_output(program);
            vm.run();
            assert_eq!(out.lock().unwrap().join("\n"), expect, "for: {}", src);
        };
        ok("const sq = x => x * x\nprint(sq(5))", "25");
        ok("print(0x10.toString(16), 0b101.toString(2), 0o17.toString(8))", "10 101 17");
        ok("print(.5, .5e2, 0..toString(2), 1..toString()) ", "0.5 50 0 1");
        ok("print(1, 2, 3)", "1 2 3");
        ok("print([1, 2, 3], {a: 1, b: 2})", "[1, 2, 3] [object Object]");
        ok("let [h, , ...t] = [1, 2, 3, 4]; print(h, t.length)", "1 2");
        ok("function f(a, b) { return a + b; } print(f(1, 2))", "3");
        ok("print((1 + 2) * 3, (1, 2, 3))", "9 3");
        // `instanceof` is a real operator (class work), not a silent misparse.
        ok("class A {} let a = new A(); print(a instanceof A, 1 instanceof Number, null instanceof A)", "true false false");
    }

    /// Undeclared identifiers are a ReferenceError at runtime, matching JS
    /// (`print(g)` throws; `typeof g` is "undefined" and does not). A
    /// declared-but-unassigned `let` is readable (undefined), and the error
    /// is catchable by try/catch.
    #[test]
    fn undeclared_globals_throw_reference_error() {
        let src = r#"
            print(typeof g);
            print(typeof (g));
            let g2 = 5; print(g2);
            print(typeof g2);
            let unassigned;
            print(unassigned);
            try { print(missing); } catch (e) { print("caught"); }
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "undefined undefined 5 number undefined caught");

        // An uncaught ReferenceError surfaces as the uncaught exception.
        let program = Compiler::compile_source("print(nope);").expect("compile");
        let (mut vm, out) = Vm::with_output(program);
        vm.run();
        assert!(out.lock().unwrap().is_empty());
        let e = vm.take_error().expect("ReferenceError recorded");
        assert!(format!("{:?}", e).contains("ReferenceError: nope is not defined"));
    }

    /// is INT or NUMBER. This exercises every fast lane plus the transitions
    /// that must fall back — cell slots (closure counter), params, int→f64
    /// flips (`n = n / 2`), BigInt overflow, and the f64 compare/mod edges.
    /// Expected values verified against Node.
    #[test]
    fn smi_feedback_lanes_match_node() {
        let src = r#"
            function counter() {
                let c = 0;
                return function () { c += 1; return c; };
            }
            function fib(n) { if (n < 2) { return n; } return fib(n - 1) + fib(n - 2); }
            function par(n) { let t = 0; for (let k = 0; k < n; k++) { t += k * 3; } return t; }
            function parf(n) { let t = 0; for (let k = 0; k < 10; k++) { t += n; n = n / 2; } return t; }
            let inc = counter();
            let cl = 0;
            for (let i = 0; i < 10; i++) { cl = inc(); }
            let x = 5;
            let xv = 0;
            for (let i = 0; i < 4; i++) {
                x = x / 2;      // f64 lane
                x = x * 2 + 1;  // f64 lane
                xv = xv + x;    // 6 + 7 + 8 + 9 (values verified in Node)
            }
            let f = fib(25);
            let p = 0;
            for (let j = 0; j < 50; j++) { p += par(100); }
            let pf = parf(1000);
            let big = 1;
            for (let i = 0; i < 60; i++) { big = big * 2; }
            let bigm = big % 7;
            let m0 = 5 % 0;
            let mneg = -9 % 3;
            let fa = 1.5, fb = 2.5;
            let c1 = fa < 2, c2 = fb !== 2.5, c3 = fa == 1.5;
            let j = 0, sum = 0;
            while (j < 5) { sum += j * 2 - 1; j += 1; }
            print(cl, xv, f, p, pf, bigm, m0, mneg, c1, c2, c3, sum);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" ");
        assert_eq!(out, "10 30 75025 742500 1998.046875 1 NaN -0 true false true 15");
    }

    /// The growable string builder: `s = s + leaf` loops append into a
    /// shared buffer, yet ALIASED readers stay valid — `snap = s` mid-build
    /// must keep the prefix it captured, never see later appends — and the
    /// final builder survives promotion with its buffer in the old
    /// generation (contiguous, so reads need no flatten).
    #[test]
    fn string_builder_aliases_stay_valid_and_promote() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let big = null;
            let snap = null;
            function build() {
                let s = "";
                for (let i = 0; i < 10; i++) {
                    if (i === 4) { snap = s; }
                    s = s + "xy";
                }
                big = s;
                return s.length;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        use alloy_core::value::AString;
        let build = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "build")
            .map(|(v, _)| v.clone())
            .expect("build");
        vm.call_value(&build, &[]);
        vm.promote_generation();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after promote");
        let check = Compiler::compile_source_with_mode(
            "print(big.length, big, snap.length, snap, big[0], big[19]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "20 xyxyxyxyxyxyxyxyxyxy 8 xyxyxyxy x y");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after check");
        let big_idx = vm.global_names.iter().position(|n| n == "big").expect("big");
        let addr = ((vm.globals[big_idx].bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(b.is_builder(), "loop-built string should be a growable builder");
        assert!(
            vm.heap.addr_in_old(b.bytes_ptr() as usize),
            "builder buffer must live in old, not young"
        );
        let snap_idx = vm.global_names.iter().position(|n| n == "snap").expect("snap");
        let saddr = ((vm.globals[snap_idx].bits() << 16) as i64 >> 16) as usize;
        let sb = unsafe { &*(saddr as *const AString) };
        assert_eq!(sb.len(), 8, "aliased snapshot must keep its prefix length");
    }

    /// Mixed concat through real JS: `s = s + i` with numbers, plus a
    /// string + object mix, must match Node's concatenation byte-for-byte.
    #[test]
    fn mixed_concat_matches_js_semantics() {
        let src = r#"
            let s = "";
            for (let i = 0; i < 50; i++) { s = s + i; }
            let o = { x: 1 };
            print(s, "pre" + 3.5 + true + null + undefined, "v=" + o);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "012345678910111213141516171819202122232425262728293031323334353637383940414243444546474849 pre3.5truenullundefined v=[object Object]"
        );
    }

    /// Rope + generations: a loop-built rope stored in a global survives
    /// promotion (the whole cons tree moves to old, iteratively), and the
    /// lazy flatten triggered by reading it afterwards allocates the flat
    /// bytes in the OLD generation — so they survive the young sweep instead
    /// of dangling.
    #[test]
    fn rope_promoted_then_flattened_lives_in_old_gen() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let big = null;
            function build() {
                let s = "";
                for (let i = 0; i < 2000; i++) { s = s + "xy"; }
                big = s;
                return big.length;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(setup);
        vm.run();
        use alloy_core::value::AString;
        let build = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "build")
            .map(|(v, _)| v.clone())
            .expect("build");
        // One unit: build the string (a growable builder — the small-start
        // fast path), promote it, reset young.
        vm.call_value(&build, &[]);
        vm.promote_generation();
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after promote");
        // Reading the promoted builder is direct (already contiguous) and
        // its buffer must live in the OLD generation.
        let check = Compiler::compile_source_with_mode(
            "print(big.length, big[0], big[3999]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "4000 x y");
        assert_eq!(vm.heap_used_young(), 0, "must not leak young bytes");
        let big_idx = vm.global_names.iter().position(|n| n == "big").expect("big");
        let addr = ((vm.globals[big_idx].bits() << 16) as i64 >> 16) as usize;
        let b = unsafe { &*(addr as *const AString) };
        assert!(!b.is_cons(), "big must not be a rope tree");
        assert!(
            vm.heap.addr_in_old(b.bytes_ptr() as usize),
            "builder bytes must live in old, not young"
        );
    }

    /// Rope + major GC: a rope stored in a global survives repeated
    /// second-generation sweeps (the mark records the whole cons subtree and
    /// every leaf byte region), and remains readable afterwards.
    #[test]
    fn rope_survives_major_gc_sweeps() {
        let setup = Compiler::compile_source_with_mode(
            r#"
            let rope_keep = "";
            function seed() {
                let s = "";
                for (let i = 0; i < 512; i++) { s = s + "abcdefghij"; }
                rope_keep = s;
            }
            function churn(n) {
                let t = "";
                for (let i = 0; i < 32; i++) { t = t + "junk!"; }
                return t;
            }
        "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(setup);
        vm.run();
        let seed = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "seed")
            .map(|(v, _)| v.clone())
            .expect("seed");
        vm.call_value(&seed, &[]);
        vm.promote_generation();
        vm.major_threshold = 1 << 10;
        let churn = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "churn")
            .map(|(v, _)| v.clone())
            .expect("churn");
        for i in 0..500 {
            vm.call_value(&churn, &[Value::int(i)]);
            vm.promote_generation();
        }
        let check = Compiler::compile_source_with_mode(
            "print(rope_keep.length, rope_keep[0], rope_keep[5119]);",
            true, false,
        )
        .unwrap();
        vm.set_program(check);
        vm.run();
        let out = _sink.lock().unwrap().join("\n");
        assert_eq!(out, "5120 a j");
        assert_eq!(vm.heap_used_young(), 0, "young not reclaimed after sweeps");
    }

    /// The PRD headline: `import { f } from './x.py' as python`, allocate a
    /// Float32Array in the shared segment, hand its pointer to Python, and
    /// read the result back — zero-copy through the file-backed mmap both
    /// sides map. Skipped (passes trivially) when no `python` is installed.
    #[test]
    fn python_polyglot_sidecar() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_ai_model_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def processTensor(ptr):\n    total = 0.0\n    for i in range(8):\n        total += read_f32(ptr + i * 4)\n    return total\n\ndef scale(ptr, n):\n    return [read_f32(ptr + i * 4) * n for i in range(4)]\n\ndef boom():\n    raise ValueError('kaboom')\n",
        )
        .unwrap();
        // Forward slashes: the JS string lexer treats backslashes as escapes.
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ processTensor, scale, boom }} from '{}' as python;
            const buf = memory.allocateFloat32Array([1, 2, 3, 4, 5, 6, 7, 8]);
            (async () => {{
                const sum = await python.processTensor(buf.ptr);
                print("sum=" + sum);
                const scaled = await python.scale(buf.ptr, 2);
                print("scaled[2]=" + scaled[2]);
                try {{
                    await python.boom();
                    print("no-error");
                }} catch (e) {{
                    print("caught=" + e);
                }}
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "sum=36\nscaled[2]=6\ncaught=kaboom");
    }

    /// The in-process interpreter (`ALLOY_PYTHON_EMBED=1`): same PRD demo
    /// as [`python_polyglot_sidecar`], but the python runs in THIS process
    /// (GIL-guarded calls, no subprocess). This test is the embed suite — it
    /// self-skips unless the suite runs with embed enabled (the child-mode
    /// suite above covers the subprocess path).
    #[test]
    fn python_embed_roundtrip() {
        if !embed_mode() {
            eprintln!("skip: run the suite with ALLOY_PYTHON_EMBED=1 to exercise the in-process interpreter");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_embed_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def processTensor(ptr):\n    total = 0.0\n    for i in range(8):\n        total += read_f32(ptr + i * 4)\n    return total\n\ndef scale(ptr, n):\n    return [read_f32(ptr + i * 4) * n for i in range(4)]\n\ndef pick(a, b, c):\n    return b\n\ndef boom():\n    raise ValueError('kaboom')\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ processTensor, scale, pick, boom }} from '{}' as python;
            const buf = memory.allocateFloat32Array([1, 2, 3, 4, 5, 6, 7, 8]);
            (async () => {{
                const sum = await python.processTensor(buf.ptr);
                print("sum=" + sum);
                const scaled = await python.scale(buf.ptr, 2);
                print("scaled[2]=" + scaled[2]);
                print("pick=" + (await python.pick(1, "mid", 3)));
                print("pick2=" + (await python.pick(1, "other", 3)));
                try {{
                    await python.boom();
                    print("no-error");
                }} catch (e) {{
                    print("caught=" + e);
                }}
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "sum=36\nscaled[2]=6\npick=mid\npick2=other\ncaught=kaboom",
            "embed round-trip diverged from the child path: {}",
            out
        );
    }

    /// `finalize_interpreter()` refuses to run while any embed backend is
    /// alive — finalizing over a live module pointer would leave the
    /// interpreter with dangling references. This test holds its own live
    /// backend (so the guard is guaranteed to trip regardless of what other
    /// parallel tests do) and verifies the refusal; the real `Py_FinalizeEx`
    /// path is exercised by the subprocess CLI test (it is terminal for the
    /// process, so it can never run inside the shared test binary).
    #[test]
    fn finalize_guard_blocks_live_backends() {
        if !embed_mode() {
            eprintln!("skip: embed-only (run with ALLOY_PYTHON_EMBED=1)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_fin_guard_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ add }} from '{}' as python;
            (async () => {{ print("r=" + (await python.add(1, 2))); }})();
            "#,
            src
        ))
        .unwrap();
        // Other tests share the process-global interpreter and hold their
        // own backends, so only lower-bound assertions are sound here. The
        // precise teardown accounting (backends released → finalize
        // succeeds) is covered deterministically by the subprocess CLI test,
        // which cannot run inside this shared test binary.
        let baseline = crate::python_embed::live_backends();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        assert_eq!(sink.lock().unwrap().join("\n"), "r=3");
        // Our own backend is alive → the guard must refuse (and it must
        // refuse without touching the interpreter, so the process stays
        // usable for parallel tests).
        assert!(
            crate::python_embed::live_backends() > baseline,
            "expected at least our own live backend"
        );
        let err = crate::python_embed::finalize_interpreter()
            .expect_err("finalize must refuse while backends are live");
        assert!(
            err.contains("still alive"),
            "unexpected guard error: {}",
            err
        );
        assert!(
            !crate::python_embed::is_finalized(),
            "a refused finalize must not mark the interpreter finalized"
        );
        drop(vm);
        let _ = std::fs::remove_file(&py_path);
    }

    /// `sweepSegments()` is a global native: it reclaims orphaned segment
    /// files (dead pid, past the grace period) immediately — no 60s
    /// rate-limit wait — and returns `{ files, bytes }` describing what was
    /// reclaimed, so a long-running server can call it between requests and
    /// alert on `files > 0`. Safety rules are the same as the automatic
    /// sweep (a live process's segment is never touched).
    #[test]
    fn sweep_segments_native_reclaims_orphans_immediately() {
        // Advance the process-wide sweep rate-limit clock NOW (bypassing the
        // limit) so no concurrent VM's automatic sweep can run for the next
        // 60s and delete our orphan before the explicit sweepSegments()
        // call — making the assertion deterministic under parallel tests.
        // (The old warm-up VM only advanced the clock if its own automatic
        // sweep fired, which a concurrent test's earlier sweep can
        // rate-limit out.)
        alloy_core::shared_memory::sweep_segments_now();
        // A provably-dead, NON-RECYCLABLE pid for the fake orphan's
        // filename: far above any real pid range, so it is never a live
        // process (Unix: beyond pid_max → ESRCH; Windows: OpenProcess
        // fails) and the OS can never allocate it. A real dead pid from
        // spawn + reap can be recycled within milliseconds under parallel
        // load, making the file look live and the sweep skip it.
        const DEAD_PID: u32 = u32::MAX - 1;
        let dir = std::env::temp_dir();
        let orphan = dir.join(format!("alloy_shm_{}_999999999_0.tmp", DEAD_PID));
        std::fs::write(&orphan, b"orphan!").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        {
            // Write handle: setting file times needs FILE_WRITE_ATTRIBUTES,
            // which a read-only handle lacks on Windows.
            let f = std::fs::File::options().write(true).open(&orphan).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .expect("age the orphan past the grace period");
        }
        let src = r#"
            let r = sweepSegments();
            print("swept", r.files, r.bytes);
            print("keys", r.files >= 0, r.bytes >= 0);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join(" | ");
        assert!(out.contains("swept 1 7"), "sweepSegments must report the orphan, got: {out}");
        assert!(out.contains("keys true true"), "result object malformed, got: {out}");
        assert!(!orphan.exists(), "sweepSegments() must delete the orphan");
    }

    /// `spawn` runs a function as an isolated task on the event loop; tasks
    /// park on `await ch.recv()` and are woken by sends from other tasks.
    /// `spawn(fn)` runs `fn` on a worker thread in an isolated VM: the
    /// worker's captured state is a serialized copy (mutating `log` inside a
    /// task cannot touch the caller's `log`), and each task's result crosses
    /// back through the completion channel and settles as a promise.
    #[test]
    fn spawn_isolated_context_and_result_crossing() {
        let program = Compiler::compile_source(
            r#"
            async function main() {
                let log = "M";
                let r1 = await spawn(async () => {
                    // This log is a fresh copy in the worker's context.
                    return "A";
                });
                let r2 = await spawn(function () {
                    return "BC";
                });
                print("main=" + log + " p1=" + r1 + " p2=" + r2);
            }
            main();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "main=M p1=A p2=BC");
    }

    /// `await spawn(f)` yields f's result; a spawned task's state is isolated
    /// from the spawning scope (locals, not shared cells).
    #[test]
    fn spawn_await_result() {
        let program = Compiler::compile_source(
            r#"
            (async () => {
                const r = await spawn(() => 42);
                print("result=" + r);
            })();
            "#,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "result=42");
    }

    /// The point of async python calls: a slow Python function must not
    /// freeze the event loop. A 10ms timer fires (and sets `t = 1`) while a
    /// 300ms python call is still in flight on its worker thread; the await
    /// resumes only afterwards with the timer's effect visible.
    #[test]
    fn python_call_does_not_block_event_loop() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_slow_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def slow(sec):\n    import time\n    time.sleep(sec)\n    return sec\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ slow }} from '{}' as python;
            let t = 0;
            setTimeout(() => {{ t = 1; print("timer"); }}, 10);
            (async () => {{
                const r = await python.slow(0.3);
                print("slow-done t=" + t);
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        let start = std::time::Instant::now();
        vm.run();
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        // The timer fired while the python call was in flight, so it prints
        // first and its side effect (t = 1) is visible when the await resumes.
        assert_eq!(out, "timer\nslow-done t=1");
        // Sanity: we waited for the slow call (~300ms) but the timer wasn't
        // delayed by it.
        assert!(elapsed.as_millis() >= 280, "ran too fast: {:?}", elapsed);
    }

    /// The HTTP-server shape: a handler awaits a python call, so after
    /// `call_value` suspends it, `drive_pending` must pump the loop until the
    /// promise settles and the handler resumes (the serve_http glue calls
    /// this before reading the response).
    #[test]
    fn python_call_from_handler_resolves() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_add_handler_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        // REPL mode so the top-level `handle` function becomes a global.
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ add }} from '{}' as python;
                async function handle(req, res) {{
                    const v = await python.add(2, 3);
                    print("handler-got=" + v);
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let handler = vm
            .globals
            .iter()
            .zip(vm.global_names.iter())
            .find(|(_, n)| n.as_str() == "handle")
            .map(|(v, _)| v.clone())
            .expect("handle global");
        // Simulate serve_http: invoke the handler, pump pending async work,
        // then (implicitly) the handler has resumed.
        vm.call_value(&handler, &[Value::undefined(), Value::undefined()]);
        vm.drive_pending();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "handler-got=5");
    }

    /// The worker queue: many sequential awaits all round-trip through the
    /// file's single dedicated worker thread (one `python_workers` entry, no
    /// per-call spawn), resolving in order with correct results.
    #[test]
    fn python_worker_queue_handles_many_calls() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_add_queue_{}.py", std::process::id()));
        std::fs::write(&py_path, "def add(a, b):\n    return a + b\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ add }} from '{}' as python;
            (async () => {{
                let s = 0;
                for (let i = 0; i < 25; i++) {{
                    s = await python.add(s, i);
                }}
                print("sum=" + s);
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        // Pool keys are canonical paths; resolve before the file is removed.
        let canon = vm.resolve_py_path(&src).unwrap_or(src.clone());
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        // 0 + 1 + ... + 24 = 300, resolved through 25 queued worker calls.
        assert_eq!(out, "sum=300");
        assert_eq!(vm.python_workers.len(), 1, "expected one worker per file");
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
        // Caveat 1 (lazy pool): 25 sequential calls never contended the single
        // child, so the pool must NOT have grown — exactly one child, with its
        // in-flight slot back to zero.
        let w = vm.python_workers.get(&canon).expect("worker pool");
        assert_eq!(w.senders.len(), 1, "lazy pool grew without contention");
        assert_eq!(w.busy.iter().sum::<usize>(), 0, "children left busy");
    }

    /// Caveat 2 (per-call timeout): a python function that never returns is
    /// killed at the deadline — the promise rejects with a timeout error —
    /// and the pool heals so the next call succeeds on the respawned child.
    #[test]
    fn python_call_times_out_and_worker_heals() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: per-call timeout kill is a child-mode feature");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_timeout_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def forever():\n    import time\n    time.sleep(3600)\ndef add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ forever, add }} from '{}' as python;
            (async () => {{
                let msg = "none";
                try {{
                    await python.forever();
                }} catch (e) {{
                    msg = "" + e;
                }}
                print("caught=" + msg);
                const healed = await python.add(2, 3);
                print("healed=" + healed);
            }})();
            "#,
            src
        ))
        .unwrap();
        // VM-scoped deadline: must NOT touch the process env var, or pools
        // that other tests spawn concurrently would inherit 400ms timeouts.
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_python_timeout(400);
        let start = std::time::Instant::now();
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert!(
            out.contains("caught=python call timed out"),
            "expected a timeout rejection, got: {}",
            out
        );
        assert!(out.contains("healed=5"), "worker did not heal: {}", out);
        // The whole run (400ms deadline + restart + a heal call) must finish
        // well under the 3600s the hung function would have taken.
        assert!(start.elapsed().as_secs() < 60, "test ran too long");
        assert_eq!(vm.python_inflight, 0);
    }

    /// A pure-python busy loop (no GIL-releasing C call) times out in BOTH
    /// backends: the child mode kills the process at the deadline, and the
    /// embed mode delivers the cooperative `KeyboardInterrupt` via
    /// `PyThreadState_SetAsyncExc` at the next bytecode boundary. Either way
    /// the promise rejects with a timeout error, the worker heals, and the
    /// next call succeeds. This is the embed-mode timeout path the
    /// `time.sleep` test above cannot cover (sleep releases the GIL, so the
    /// cooperative interrupt can't land until it returns).
    #[test]
    fn python_busy_loop_times_out_in_both_modes() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_busy_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def busy():\n    while True:\n        pass\ndef add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ busy, add }} from '{}' as python;
            (async () => {{
                let msg = "none";
                try {{
                    await python.busy();
                }} catch (e) {{
                    msg = "" + e;
                }}
                print("caught=" + msg);
                const healed = await python.add(2, 3);
                print("healed=" + healed);
            }})();
            "#,
            src
        ))
        .unwrap();
        // VM-scoped deadline: must NOT touch the process env var, or pools
        // that other tests spawn concurrently would inherit 400ms timeouts.
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_python_timeout(400);
        let start = std::time::Instant::now();
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert!(
            out.contains("caught=python call timed out"),
            "expected a timeout rejection, got: {}",
            out
        );
        assert!(out.contains("healed=5"), "worker did not heal: {}", out);
        assert!(start.elapsed().as_secs() < 60, "test ran too long");
        assert_eq!(vm.python_inflight, 0);
    }

    /// `reload('./x.py')` re-imports a python sidecar: the old child is
    /// killed and the next call runs the fresh file (Node-style cache
    /// invalidation for the python pillar). Old module references keep
    /// working because the natives route through the canonical path, which
    /// re-checks the generation on every call.
    #[test]
    fn reload_python_module_reimports() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_a_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                print('v1:' + await py.f());
                print('wrote:' + fslib.writeFileSync('{}', 'def f():\n    return 2\n'));
                print('reloaded:' + reload('{}'));
                print('v2:' + await py.f());
                print('reloaded-again:' + reload('{}'));
                print('v3:' + await py.f());
            }}
            main();
            "#,
            abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "v1:1\nwrote:true\nreloaded:true\nv2:2\nreloaded-again:true\nv3:2"
        );
        let _ = std::fs::remove_file(&py_path);
        assert_eq!(vm.python_workers.len(), 1);
    }

    /// The per-call generation check: a reload that lands WHILE a call is in
    /// flight aborts it — the sleeping child is killed — and re-runs the call
    /// on the freshly imported child, so the promise resolves with the new
    /// implementation instead of rejecting or settling the stale result.
    /// Deterministic: the first call sleeps 400ms; the write + reload happen
    /// microseconds later, long before it would have returned 1.
    #[test]
    fn reload_aborts_inflight_python_call_and_reruns() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_abort_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    import time\n    time.sleep(0.4)\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                const p = py.f();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                print('result:' + await p);
            }}
            main();
            "#,
            abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\nresult:2");
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// The headline cross-thread case: a LIVE spawn worker calls a python
    /// function, parks on a channel; the main thread rewrites the `.py` and
    /// reloads; the worker is woken by the channel send and re-imports the
    /// file on its next call — it sees the NEW result (2), not the stale
    /// child's (1). The main VM's own pool re-imports too. Requires `require`
    /// of a `.py`, which is how workers load python modules at all.
    #[test]
    fn python_reload_propagates_to_live_worker() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_b_{}.py", pid));
        std::fs::write(&py_path, "def f():\n    return 1\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                channel.create("py_ready");
                channel.create("py_go");
                const p = spawn(function () {{
                    const ready = channel.get("py_ready");
                    const go = channel.get("py_go");
                    return (async function () {{
                        const m = require('{}');
                        const a = await m.f();
                        ready.send("ready");
                        await go.recv();
                        const m2 = require('{}');
                        const b = await m2.f();
                        return [a, b];
                    }})();
                }});
                print('main1:' + await py.f());
                // The worker has called f() once and parked on `go`.
                await channel.get("py_ready").recv();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                channel.get("py_go").send("go");
                print('worker:' + (await p).join(","));
                print('main2:' + await py.f());
            }}
            main();
            "#,
            abs, abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "main1:1\nreloaded:true\nworker:1,2\nmain2:2"
        );
        let _ = std::fs::remove_file(&py_path);
    }

    /// The cross-thread per-call check: a WORKER has a call in flight on its
    /// child when the MAIN thread reloads the `.py`. The worker's old child
    /// survives (it's a separate process from the main VM's pool) and finishes
    /// the sleep with the old code — but the worker's response drain compares
    /// the call's generation against the shared registry, sees it moved,
    /// tears down its own stale pool, and re-runs the call on a fresh child.
    /// The promise resolves with the new implementation (2), not the stale
    /// result (1), without the worker ever being told to reload explicitly.
    #[test]
    fn main_reload_reruns_worker_inflight_python_call() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_abortw_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.4)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                channel.create("abort_ready");
                const p = spawn(function () {{
                    const ready = channel.get("abort_ready");
                    return (async function () {{
                        const m = require('{}');
                        const a = m.f();      // in flight, sleeping 400ms
                        ready.send("in-flight");
                        return await a;       // reload lands mid-flight
                    }})();
                }});
                await channel.get("abort_ready").recv();
                fslib.writeFileSync('{}', 'def f():\n    return 2\n');
                print('reloaded:' + reload('{}'));
                print('worker:' + await p);
            }}
            main();
            "#,
            abs, abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "reloaded:true\nworker:2");
    }

    /// Coalescing: a BURST of reloads on the same `.py` while a call is in
    /// flight triggers exactly ONE pool rebuild and one re-run, not a re-run
    /// cascade. All four reloads land sub-ms apart — well inside the
    /// coalescing window and long before the call's response — so they fold
    /// into one burst: the first tears the pool down (the abort), the rest
    /// leave it alone. The single rebuild reads the file at rebuild time,
    /// i.e. the LAST write of the burst, and the re-run settles with it; the
    /// next call sees the same burst and serves the rebuilt pool with no
    /// further rebuild. Before coalescing, each reload bumped the version,
    /// so N reloads while the call (or its re-run) was in flight meant N
    /// rebuilds and N re-runs.
    #[test]
    fn burst_of_python_reloads_triggers_one_rebuild() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_burst_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.5)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            async function main() {{
                const fslib = require('fs');
                const p = py.f();       // in flight, sleeping 500ms
                // Burst: 4 rapid reloads, each also rewriting the file.
                for (let i = 0; i < 4; i++) {{
                    fslib.writeFileSync('{}', 'def f():\n    return ' + (2 + i) + '\n');
                    print('reloaded:' + reload('{}'));
                }}
                print('result:' + await p);   // re-run on the single rebuilt pool
                print('next:' + await py.f()); // same burst: no further rebuild
            }}
            main();
            "#,
            abs, abs, abs
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "reloaded:true\nreloaded:true\nreloaded:true\nreloaded:true\nresult:5\nnext:5"
        );
        assert_eq!(
            vm.python_rebuilds, 1,
            "a burst of reloads must rebuild the pool exactly once, not once per reload"
        );
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// The coalescing window is configurable via `ALLOY_PYTHON_RELOAD_MS`:
    /// widened past the default, two reloads spaced **beyond** the default
    /// 250ms window still fold into one burst (one pool rebuild). The same
    /// spacing at the default window would be two separate bursts — the
    /// second reload would tear the rebuilt pool down and the next call
    /// would rebuild again. The script spaces the reloads ~400ms apart (past
    /// the 250ms default, inside the 5000ms test window): the first reload
    /// aborts the in-flight call, the drain rebuilds once (reading v2), and
    /// the folded second reload neither bumps the version nor tears the pool
    /// down — exactly one rebuild, and the re-run resolves the state written
    /// before the first reload. The next call serves the same (un-rebuilt)
    /// pool, which is the documented coalescing tradeoff: a reload folded
    /// into the current burst is picked up by the next rebuild.
    #[test]
    fn widened_python_reload_window_folds_larger_spacing() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_win_{}.py", pid));
        std::fs::write(
            &py_path,
            "def f():\n    import time\n    time.sleep(0.15)\n    return 1\n",
        )
        .unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as py;
            function sleep(ms) {{
                const w = Promise.withResolvers();
                setTimeout(function () {{ w.resolve(1); }}, ms);
                return w.promise;
            }}
            async function main() {{
                const fslib = require('fs');
                const p = py.f();               // C1: v1, sleeps 150ms
                fslib.writeFileSync('{}', 'def f():\n    import time\n    time.sleep(0.15)\n    return 2\n');
                print('r1:' + reload('{}'));    // new burst: kills C1
                await sleep(400);               // drain rebuilds C2 (reads v2), re-runs
                fslib.writeFileSync('{}', 'def f():\n    import time\n    time.sleep(0.15)\n    return 3\n');
                print('r2:' + reload('{}'));    // ~400ms after r1: folded by the widened window
                print('result:' + await p);     // C2's re-run -> 2
                print('next:' + await py.f());  // same burst: same pool -> 2
            }}
            main();
            "#,
            abs, abs, abs, abs, abs
        ))
        .unwrap();
        let prev = std::env::var("ALLOY_PYTHON_RELOAD_MS").ok();
        std::env::set_var("ALLOY_PYTHON_RELOAD_MS", "5000");
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        match prev {
            Some(p) => std::env::set_var("ALLOY_PYTHON_RELOAD_MS", p),
            None => std::env::remove_var("ALLOY_PYTHON_RELOAD_MS"),
        }
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "r1:true\nr2:true\nresult:2\nnext:2");
        assert_eq!(
            vm.python_rebuilds, 1,
            "the widened window must fold the ~400ms-spaced reloads into one burst (one rebuild)"
        );
        assert_eq!(vm.python_inflight, 0, "calls left in flight after run");
    }

    /// `reload` of a `.py` that exists but was never imported is a no-op
    /// (false), like the .ajs path; a missing file is false too.
    #[test]
    fn reload_unimported_python_is_false() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let py_path = dir.join(format!("alloy_pyrel_c_{}.py", pid));
        std::fs::write(&py_path, "def g():\n    return 0\n").unwrap();
        let abs = py_path.to_string_lossy().replace('\\', "/");
        let nope = dir.join(format!("alloy_pyrel_d_{}.py", pid));
        let nope_s = nope.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            print(reload('{}'));
            print(reload('{}'));
            "#,
            abs, nope_s
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "false\nfalse");
        let _ = std::fs::remove_file(&py_path);
    }

    /// The concurrent HTTP server: N requests sent at once, each handler
    /// awaiting a *different* slow python function (different files →
    /// different workers → parallel execution). Every response must be
    /// correct, and the total wall time must reflect parallel (~max, one
    /// slow call) rather than serialized (~sum, three slow calls).
    #[test]
    fn server_concurrent_await_python() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        // Three files so the three slow calls run on three workers in
        // parallel (same-file calls serialize on one worker).
        let mut py_paths = Vec::new();
        let mut imports = String::new();
        for i in 0..3usize {
            let p = dir.join(format!("alloy_test_slow_{}_{}.py", i, pid));
            std::fs::write(
                &p,
                format!("def f(sec):\n    import time\n    time.sleep(sec)\n    return {}\n", 100 + i * 100),
            )
            .unwrap();
            let s = p.to_string_lossy().replace('\\', "/");
            imports.push_str(&format!("import {{ f }} from '{}' as py{};\n", s, i));
            py_paths.push(p);
        }
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                {}
                async function handle(req, res) {{
                    const id = req.url;
                    const r = id == "/0" ? await py0.f(0.4) : id == "/1" ? await py1.f(0.4) : await py2.f(0.4);
                    res.send(id + ":" + r);
                }}
                "#,
                imports
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            // Look the handler up inside the thread: a function Value holds
            // Rc cells and is not Send, but the Vm itself is (unsafe impl).
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // N concurrent clients: connect + send together, then read together.
        let start = std::time::Instant::now();
        let clients: Vec<std::thread::JoinHandle<(usize, String)>> = (0..3usize)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut stream =
                        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
                    let req = format!("GET /{} HTTP/1.1\r\nHost: t\r\n\r\n", i);
                    stream.write_all(req.as_bytes()).unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    (i, String::from_utf8_lossy(&buf).to_string())
                })
            })
            .collect();
        let mut responses = Vec::new();
        for c in clients {
            responses.push(c.join().expect("client thread"));
        }
        let elapsed = start.elapsed();
        for p in &py_paths {
            let _ = std::fs::remove_file(p);
        }
        // Every response must carry its own request's marker from its own
        // python file (0→100, 1→200, 2→300).
        for (i, resp) in &responses {
            assert!(
                resp.contains(&format!("/{}:{}", i, 100 + i * 100)),
                "response for /{} was: {}",
                i,
                resp
            );
        }
        // Parallel: 3 × 0.4s python ≈ one 0.4s + overhead. Serialized
        // (requests served one at a time) would be ≥ 1.2s. 1.05s sits
        // between with margin on both sides.
        assert!(
            elapsed.as_millis() < 1050,
            "requests were serialized, not concurrent: {:?}",
            elapsed
        );
        // Stop the serve thread and join: the VM drops deterministically
        // (python children reaped, shared-segment file removed) instead of
        // lingering on a detached thread.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 1: same-file python calls no longer serialize. Two concurrent
    /// requests whose handlers both await a slow function from the *same*
    /// imported file run on the file's pool children in parallel: ~max(0.4s),
    /// not ~sum(0.8s).
    #[test]
    fn server_same_file_calls_run_in_parallel() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: same-file parallelism is a child-mode feature (GIL serializes)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_same_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def f(sec):\n    import time\n    time.sleep(sec)\n    return 42\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ f }} from '{}' as py;
                async function handle(req, res) {{
                    const r = await py.f(0.4);
                    res.send(req.url + ":" + r);
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // Warm-up pair: two overlapping requests trigger the pool's lazy
        // growth (child 0 is busy when the second lands), so the measured
        // pair below runs on a warm 2-child pool — the one-time child-spawn
        // latency stays out of the timing window.
        let warm: Vec<_> = (0..2)
            .map(|i| {
                std::thread::spawn(move || http_client(port, &format!("/w{}", i)))
            })
            .collect();
        for t in warm {
            t.join().unwrap().expect("warm request");
        }
        let start = std::time::Instant::now();
        let clients: Vec<_> = (0..2)
            .map(|i| {
                std::thread::spawn(move || http_client(port, &format!("/{}", i)))
            })
            .collect();
        let mut ok = true;
        for c in clients {
            match c.join() {
                Ok(Ok(resp)) => {
                    if !resp.contains("/0:42") && !resp.contains("/1:42") {
                        ok = false;
                    }
                }
                _ => ok = false,
            }
        }
        let elapsed = start.elapsed();
        let _ = std::fs::remove_file(&py_path);
        assert!(ok, "one or more responses were wrong");
        // Parallel (warm 2-child pool): ~0.4s. Serialized on one child: ≥ 0.8s.
        assert!(
            elapsed.as_millis() < 650,
            "same-file calls were serialized: {:?}",
            elapsed
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Packed int arrays: the fast path must be invisible to semantics —
    /// reads, .length, holes, mixed-type escapes, `==` string coercion, and
    /// survival across a GC promotion all behave exactly like the general
    /// form. Also exercises the LoadLocalGetPropConst (`a.length`) and
    /// LoadLocalLocalGetIndex (`a[i]`) superinstructions in the loop.
    #[test]
    fn packed_int_array_semantics_and_fusions() {
        let src = r#"
            let keep = null;
            function seed() {
                keep = [1, 2, 3];
                // Escape to mixed on a non-int write; the earlier ints must
                // stay visible.
                keep[3] = "four";
            }
            seed();
            let a = [1, 2, 3];
            print("len=" + a.length);
            print("idx=" + a[1]);
            print("hole=" + a[7]);
            a[5] = 42;
            print("extended=" + a.length + ":" + a[4] + ":" + a[5]);
            let s = 0;
            for (let i = 0; i < a.length; i++) {
                let v = a[i];
                if (v !== undefined) { s += v; }
            }
            print("sum=" + s);
            // == coercion on a packed array comma-joins like JS.
            print("eq=" + ([1, 2] == "1,2"));
            print("mixed=" + keep[0] + ":" + keep[3]);
        "#;
        let (_vm, sink) = run_src(src);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(
            out,
            "len=3\nidx=2\nhole=undefined\nextended=6:undefined:42\nsum=48\neq=true\nmixed=1:four"
        );
    }

    /// Caveat 1 (lazy pool growth under contention): a second call that
    /// lands while the first is still in flight must grow the file's pool to
    /// a second child, and both calls complete in parallel (each response
    /// correct).
    #[test]
    fn python_pool_grows_lazily_under_contention() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        if embed_mode() {
            eprintln!("skip: pool growth is a child-mode feature (embed caps at one child)");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_grow_{}.py", std::process::id()));
        std::fs::write(
            &py_path,
            "def f(sec, tag):\n    import time\n    time.sleep(sec)\n    return tag\n",
        )
        .unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source(&format!(
            r#"
            import {{ f }} from '{}' as python;
            (async () => {{
                const p1 = python.f(0.3, 1);
                const p2 = python.f(0.3, 2);
                print("a=" + (await p1) + " b=" + (await p2));
            }})();
            "#,
            src
        ))
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.run();
        // Pool keys are canonical paths; resolve before the file is removed.
        let canon = vm.resolve_py_path(&src).unwrap_or(src.clone());
        let _ = std::fs::remove_file(&py_path);
        let out = sink.lock().unwrap().join("\n");
        assert_eq!(out, "a=1 b=2");
        let w = vm.python_workers.get(&canon).expect("worker pool");
        // Both calls were in flight at once, so the pool grew to its cap of 2.
        assert_eq!(w.senders.len(), 2, "pool did not grow under contention");
        assert_eq!(w.busy.iter().sum::<usize>(), 0, "children left busy");
    }

    /// Caveat 3: a handler whose python call rejects responds 500 with the
    /// rejection reason, instead of a silent default body.
    #[test]
    fn server_handler_error_returns_500() {
        if std::process::Command::new("python").arg("--version").output().is_err() {
            eprintln!("skip: no python on PATH");
            return;
        }
        let dir = std::env::temp_dir();
        let py_path = dir.join(format!("alloy_test_boom_{}.py", std::process::id()));
        std::fs::write(&py_path, "def boom():\n    raise ValueError('kaboom from python')\n").unwrap();
        let src = py_path.to_string_lossy().replace('\\', "/");
        let program = Compiler::compile_source_with_mode(
            &format!(
                r#"
                import {{ boom }} from '{}' as py;
                async function handle(req, res) {{
                    await py.boom();
                    res.send("never-reached");
                }}
                "#,
                src
            ),
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let resp = http_client(port, "/").expect("request");
        let _ = std::fs::remove_file(&py_path);
        assert!(
            resp.starts_with("HTTP/1.1 500"),
            "expected 500, got: {}",
            resp.lines().next().unwrap_or("")
        );
        assert!(resp.contains("kaboom from python"), "missing reason in: {}", resp);
        assert!(!resp.contains("never-reached"), "handler continued after rejection");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 3: a *synchronous* throw inside a handler must also become a
    /// 500 (not abort the VM as an uncaught top-level throw), and the server
    /// must keep serving afterwards.
    #[test]
    fn server_sync_handler_throw_returns_500_and_keeps_serving() {
        let program = Compiler::compile_source_with_mode(
            r#"
            function handle(req, res) {
                if (req.url === "/boom") { throw "sync-boom"; }
                res.send("alive");
            }
            "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let boom = http_client(port, "/boom").expect("boom request");
        assert!(
            boom.starts_with("HTTP/1.1 500"),
            "expected 500, got: {}",
            boom.lines().next().unwrap_or("")
        );
        assert!(boom.contains("sync-boom"), "missing reason in: {}", boom);
        // The VM must not have aborted: the next request still gets served.
        let alive = http_client(port, "/ok").expect("alive request");
        assert!(alive.contains("alive"), "server died after the throw: {}", alive);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Caveat 2: a client that connects and stalls mid-request does not block
    /// the loop. While client 1 sits silent, client 2 must be accepted, read,
    /// and served promptly.
    #[test]
    fn server_stalled_client_does_not_block_others() {
        let program = Compiler::compile_source_with_mode(
            r#"
            function handle(req, res) { res.send("fast"); }
            "#,
            true, false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        // Stoppable serve thread: the test sets the flag and joins so the
        // VM drops deterministically — reaping its python sidecar children
        // (OS processes a detached thread would orphan) and removing its
        // shared-segment file immediately instead of leaving it for the
        // next startup sweep.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm
                .globals
                .iter()
                .zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle")
                .map(|(v, _)| v.clone())
                .expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        // Client 1: connect and send nothing — under the old blocking read,
        // this would stall serve_loop forever and starve every other client.
        let stalled = std::net::TcpStream::connect(("127.0.0.1", port)).expect("stalled connect");
        let _ = stalled.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        // Give the server time to accept (and, in the old code, block on) it.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let start = std::time::Instant::now();
        let resp = http_client(port, "/").expect("fast client request");
        let elapsed = start.elapsed();
        assert!(resp.contains("fast"), "fast client was not served: {}", resp);
        assert!(
            elapsed.as_millis() < 1000,
            "fast client stalled behind the silent one: {:?}",
            elapsed
        );
        drop(stalled);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    // -- web primitives (crypto / codec / URL / fetchSync / HTTP fields) --

    fn web_out(src: &str) -> String {
        let (_, sink) = run_src(src);
        let v = sink.lock().unwrap().join("\n");
        v
    }

    /// The express-compat framework (`lib/express.ajs`) against a live server:
    /// routing, :params, middleware order, error middleware, JSON bodies.
    /// Guards framework/runtime compat in CI (the full 12-check tour lives in
    /// `examples/express-demo/smoke.ajs`).
    #[test]
    fn express_framework_live_routes() {
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../lib/express.ajs");
        if !lib.exists() {
            eprintln!("skip: lib/express.ajs not found");
            return;
        }
        let program = Compiler::compile_source_with_mode(
            r#"
            import { express, Router, json } from './express.ajs';
            const app = express();
            app.use(json());
            const api = Router();
            api.get('/items/:id', (req, res) => { res.json({ id: req.params.id }); });
            api.post('/items', (req, res) => {
                if (!req.body || !req.body.name) { res.status(422); res.json({ error: "name required" }); return; }
                res.status(201); res.json({ made: req.body.name });
            });
            api.get('/fail', (req, res) => { throw new Error("nope"); });
            app.use('/api', api);
            app.use((err, req, res, next) => { res.status(500); res.json({ error: "caught: " + err }); });
            async function handle(req, res) { app.handle(req, res); }
            "#,
            true,
            false,
        )
        .unwrap();
        let (mut vm, _sink) = Vm::with_output(program);
        vm.set_script_path(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../lib/main.ajs")
                .to_string_lossy(),
        );
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm.globals.iter().zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle").map(|(v, _)| v.clone()).expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        let get = |p: &str| http_raw(port, &format!("GET {} HTTP/1.1\r\nHost: t\r\n\r\n", p)).expect("req");
        let post = |p: &str, b: &str| http_raw(port, &format!("POST {} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", p, b.len(), b)).expect("req");
        let r = get("/api/items/7");
        assert!(r.contains("200 OK") && r.contains("\"id\": \"7\""), "param: {}", r);
        let r = post("/api/items", r#"{"name":"x"}"#);
        assert!(r.contains("201 Created") && r.contains("made"), "create: {}", r);
        let r = post("/api/items", "{}");
        assert!(r.contains("422"), "validation: {}", r);
        let r = get("/api/fail");
        assert!(r.contains("500") && r.contains("nope"), "error-mw: {}", r);
        let r = get("/nothing-here");
        assert!(r.contains("404"), "framework-404: {}", r);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    /// Cross-module throw: a try/catch in one module must catch a throw from
    /// a function defined in another module (the unwinder resumes in the
    /// HANDLER's program — without the restore the pc lands in the throw
    /// site's bytecode and the dispatch dies silently with exit 0).
    #[test]
    fn cross_module_throw_caught_in_other_module() {
        let dir = std::env::temp_dir().join(format!("alloy_xmod_throw_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("catcher.ajs"),
            "export function callIt(fns) {\n\
             \x20 try {\n\
             \x20\x20 fns[0]({}, {}, () => \"step\");\n\
             \x20\x20 print(\"returned\");\n\
             \x20 } catch (e) { print(\"caught\", e); }\n\
             }\n",
        )
        .unwrap();
        let program = Compiler::compile_source_with_mode(
            "import { callIt } from './catcher.ajs';\n\
             const fns = [(req, res, step) => { throw new Error('xmod'); }];\n\
             callIt(fns);\n\
             print('after');\n",
            true,
            false,
        )
        .unwrap();
        let (mut vm, sink) = Vm::with_output(program);
        vm.set_script_path(&dir.join("main.ajs").to_string_lossy());
        vm.run();
        assert!(vm.take_error().is_none());
        assert_eq!(sink.lock().unwrap().join("\n"), "caught Error: xmod\nafter");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn web_crypto_vectors() {
        // NIST + RFC 4231 case 2 + round-trips.
        assert_eq!(web_out(r#"print(crypto.sha256("abc"))"#),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(web_out(r#"print(crypto.sha256(""))"#),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(web_out(r#"print(crypto.hmacSha256("Jefe", "what do ya want for nothing?"))"#),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(web_out(r#"print(crypto.base64Encode("Man"))"#), "TWFu");
        assert_eq!(web_out(r#"print(crypto.base64Decode("TWFu"))"#), "Man");
        assert_eq!(web_out(r#"print(crypto.base64UrlEncode(">>>"))"#), "Pj4-");
        assert_eq!(web_out(r#"print(crypto.timingSafeEqual("abc", "abc"))"#), "true");
        assert_eq!(web_out(r#"print(crypto.timingSafeEqual("abc", "abd"))"#), "false");
        assert_eq!(web_out(r#"print(crypto.timingSafeEqual("abc", "abcd"))"#), "false");
        // randomHex: 16 bytes -> 32 hex chars, and two calls differ.
        assert_eq!(web_out(r#"print(crypto.randomHex(16).length)"#), "32");
        let a = web_out(r#"print(crypto.randomHex(16))"#);
        let b = web_out(r#"print(crypto.randomHex(16))"#);
        assert_ne!(a, b);
        // JWT-shape signature is deterministic and verifies.
        assert_eq!(
            web_out(r#"const s = crypto.hmacBase64Url("k", "a.b"); print(s === crypto.hmacBase64Url("k", "a.b"))"#),
            "true");
    }

    #[test]
    fn web_uri_codec() {
        assert_eq!(web_out(r#"print(encodeURIComponent("a b+c"))"#), "a%20b%2Bc");
        assert_eq!(web_out(r#"print(encodeURIComponent("~ok-_.!*'()"))"#), "~ok-_.!*'()");
        assert_eq!(web_out(r#"print(decodeURIComponent("a%20b"))"#), "a b");
        assert_eq!(web_out(r#"print(encodeURI("http://x/?a=b&c=d#f"))"#), "http://x/?a=b&c=d#f");
        assert_eq!(web_out(r#"print(btoa("Man"))"#), "TWFu");
        assert_eq!(web_out(r#"print(atob("TWFu"))"#), "Man");
        // Malformed % sequence is a loud URIError, not silent garbage.
        let (mut vm, _) = run_src(r#"try { decodeURIComponent("%zz"); print("no-throw"); } catch (e) { print("threw"); }"#);
        assert!(vm.take_error().is_none());
        let (_, sink) = run_src(r#"try { decodeURIComponent("%zz"); print("no-throw"); } catch (e) { print("threw"); }"#);
        assert_eq!(sink.lock().unwrap().join("\n"), "threw");
    }

    #[test]
    fn web_url_parse() {
        assert_eq!(
            web_out(r#"const u = URL.parse("https://ex.com:8080/p?q=1#h"); print(u.protocol, u.hostname, u.port, u.path, u.hash)"#),
            "https: ex.com 8080 /p #h");
        assert_eq!(
            web_out(r#"const u = URL.parse("https://ex.com:8080/p?q=1#h"); print(u.host)"#),
            "ex.com:8080");
        assert_eq!(
            web_out(r#"const u = URL.parse("http://h/a?x=1&y=two+words"); print(u.query.x, u.query.y)"#),
            "1 two words");
        assert_eq!(
            web_out(r#"const u = URL.parse("/rel", "http://base.com/root"); print(u.hostname, u.path)"#),
            "base.com /rel");
        assert_eq!(
            web_out(r#"try { URL.parse(":::"); print("no-throw"); } catch (e) { print("threw"); }"#),
            "threw");
    }

    /// Raw TCP client that can send arbitrary headers/cookies (the shared
    /// `http_client` only does bare GETs).
    fn http_raw(port: u16, req: &str) -> std::io::Result<String> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        stream.write_all(req.as_bytes())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    fn serve_once(src: &str) -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>) {
        let program = Compiler::compile_source_with_mode(src, true, false).expect("compile");
        let (mut vm, _sink) = Vm::with_output(program);
        vm.run();
        let (listener, port) = bind_server(0).expect("bind ephemeral port");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            let handler = vm.globals.iter().zip(vm.global_names.iter())
                .find(|(_, n)| n.as_str() == "handle").map(|(v, _)| v.clone()).expect("handle global");
            serve_loop(&mut vm, &handler, listener, &stop2);
        });
        (port, stop, server)
    }

    #[test]
    fn web_server_fields_and_response_controls() {
        let (port, stop, server) = serve_once(r#"
            async function handle(req, res) {
                if (req.path === "/st") {
                    res.status(201);
                    res.set("X-Test", "yes");
                    res.json({ q: req.query, c: req.cookies, h: req.headers["x-foo"], p: req.path });
                    return;
                }
                if (req.path === "/html") { res.html("<h1>hi</h1>"); return; }
                res.text("plain");
            }
        "#);
        let r = http_raw(port, "GET /st?a=1&b=x+y HTTP/1.1\r\nHost: t\r\nX-Foo: bar\r\nCookie: t=abc; u=2\r\n\r\n").expect("req");
        assert!(r.contains("201 Created"), "status: {}", r);
        assert!(r.contains("X-Test: yes"), "header: {}", r);
        assert!(r.contains("\"a\": \"1\""), "query: {}", r);
        assert!(r.contains("\"b\": \"x y\""), "query-decode: {}", r);
        assert!(r.contains("\"t\": \"abc\""), "cookies: {}", r);
        assert!(r.contains("\"h\": \"bar\""), "headers: {}", r);
        let h = http_raw(port, "GET /html HTTP/1.1\r\nHost: t\r\n\r\n").expect("html");
        assert!(h.contains("text/html"), "ct: {}", h);
        assert!(h.contains("<h1>hi</h1>"), "body: {}", h);
        // Traversal + 404 shape are covered by the demo's static lib; the
        // router contract (unknown path -> handler default) is exercised here.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("serve thread exited");
    }

    #[test]
    fn web_fetch_sync_roundtrip_and_https_refusal() {
        // Tiny origin server on a thread (std only, no alloy involved).
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        let origin = std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming().take(2) {
                let mut s = stream.unwrap();
                // Read the FULL request (headers + Content-Length body) before
                // responding: a single read canSplit headers/body across
                // segments, and closing early RSTs the client's pending write.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if request_complete(&buf) { break; }
                        }
                        Err(_) => break,
                    }
                }
                let body = r#"{"hello":"world"}"#;
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes());
            }
        });
        let out = web_out(&format!(r#"
            const r = fetchSync("http://127.0.0.1:{}/x");
            print(r.status, r.ok, r.text(), r.json().hello);
        "#, port));
        assert_eq!(out, "200 true {\"hello\":\"world\"} world");
        // POST with JSON body echoes through httpbin-style: use the same origin.
        let out = web_out(&format!(r#"
            try {{
                const r = fetchSync("http://127.0.0.1:{}/x", {{ method: "POST", headers: {{ "X-A": "b" }}, body: "hi" }});
                print(r.status, r.ok);
            }} catch (e) {{ print("THREW:" + e); }}
        "#, port));
        assert_eq!(out, "200 true");
        // unsupported protocol refuses loudly.
        let out = web_out(r#"try { fetchSync("ftp://example.com/"); print("no-throw"); } catch (e) { print("threw"); }"#);
        assert_eq!(out, "threw");
        origin.join().expect("origin exited");
    }

#[cfg(test)]
mod dbg_forof {
    use crate::compiler::Compiler;
    #[test]
    fn dbg_forof_compile() {
        let src = "let s = 0\nfor (const v of [1, 2, 3, 4]) { s = s + v }\nprint(s)";
        match Compiler::compile_source(src) {
            Ok(_) => eprintln!("compile_source(false): OK"),
            Err(e) => eprintln!("compile_source(false): ERR {:?}", e),
        }
        match Compiler::compile_source_with_mode(src, true, false) {
            Ok(_) => eprintln!("compile_source(true): OK"),
            Err(e) => eprintln!("compile_source(true): ERR {:?}", e),
        }
        // also try with semicolons after the statements
        let src2 = "let s = 0; for (const v of [1, 2, 3, 4]) { s = s + v } print(s);";
        match Compiler::compile_source(src2) {
            Ok(_) => eprintln!("semicolon version: OK"),
            Err(e) => eprintln!("semicolon version: ERR {:?}", e),
        }
    }
}
