use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{mpsc, Arc};

use crate::bytecode::Program;
use alloy_core::heap::{ArenaHeap, HeapGuard};
use alloy_core::value::{FunctionData, PromiseStatus, Value, VmHost};

use super::core::Vm;
use super::modules::{SharedModuleRegistry, SharedPyRegistry};

impl Vm {
    pub(crate) fn vm_spawn_fn(&mut self, f: &Value, args: &[Value]) -> Value {
        let promise = self.new_promise();
        let reject = |vm: &mut Self, why: &str| {
            if let Some(pr) = promise.as_promise() {
                vm.reject_promise(pr, Value::string(why.to_string()));
            }
        };
        let Some(fn_data) = f.as_function() else {
            reject(self, "spawn: expected a function");
            return promise;
        };
        let Some(program) = self.programs.get(fn_data.program as usize) else {
            reject(self, "spawn: function's program is gone");
            return promise;
        };
        let program_bytes = match program.to_bytes() {
            Ok(b) => b,
            Err(_) => {
                reject(self, "spawn: cannot serialize function program");
                return promise;
            }
        };
        let mut payload = Vec::with_capacity(program_bytes.len() + 64);
        payload.extend_from_slice(&(program_bytes.len() as u32).to_be_bytes());
        payload.extend_from_slice(&program_bytes);
        payload.extend_from_slice(&(fn_data.ptr as u32).to_be_bytes());
        payload.extend_from_slice(&(fn_data.cells.len() as u32).to_be_bytes());
        for c in &fn_data.cells {
            write_spawn_value(&mut payload, &c.borrow(), true, fn_data.program);
        }
        payload.extend_from_slice(&(args.len() as u32).to_be_bytes());
        for a in args {
            write_spawn_value(&mut payload, a, true, fn_data.program);
        }
        let id = self.next_spawn_id;
        self.next_spawn_id += 1;
        self.spawn_inflight.insert(id, promise.clone());
        self.spawn_pending += 1;
        let tx = self.spawn_tx.clone();
        let registry = self.registry.clone();
        let py_registry = self.py_registry.clone();
        let dir = self.current_dir.clone();
        match std::thread::Builder::new()
            .name(format!("alloy-spawn-{}", id))
            .spawn(move || spawn_worker_thread(payload, tx, id, registry, py_registry, dir))
        {
            Ok(h) => self.spawn_workers.push(h),
            Err(e) => {
                self.spawn_pending = self.spawn_pending.saturating_sub(1);
                self.spawn_inflight.remove(&id);
                reject(self, &format!("spawn: cannot start worker thread: {}", e));
            }
        }
        promise
    }

    pub(crate) fn drain_spawn_completions(&mut self) {
        while let Ok((id, bytes)) = self.spawn_rx.try_recv() {
            self.spawn_pending = self.spawn_pending.saturating_sub(1);
            let Some(p) = self.spawn_inflight.remove(&id) else {
                continue;
            };
            let Some(pr) = p.as_promise() else { continue };
            if bytes.is_empty() {
                continue;
            }
            let mut pos = 1;
            match bytes[0] {
                // 0 = fulfilled
                0 => match crate::bytecode::decode_value(&bytes, &mut pos) {
                    Ok(v) => self.resolve_promise(pr, v),
                    Err(_) => self.reject_promise(
                        pr,
                        Value::string("spawn: result decode failed".to_string()),
                    ),
                },
                // 1 = rejected
                1 => {
                    let v = crate::bytecode::decode_value(&bytes, &mut pos)
                        .unwrap_or_else(|_| Value::undefined());
                    self.reject_promise(pr, v);
                }
                // 2 = still pending
                _ => {}
            }
        }
    }
}

pub(crate) fn make_spawn_fn() -> Value {
    Value::native(Arc::new(|args, vm| {
        let f = args.first().cloned().unwrap_or(Value::undefined());
        let rest = &args[1..];
        vm.spawn_fn(&f, rest)
    }))
}

pub(crate) fn write_spawn_value(out: &mut Vec<u8>, v: &Value, allow_fn: bool, program_id: u32) {
    if v.is_undefined() {
        out.push(0);
    } else if v.is_null() {
        out.push(1);
    } else if let Some(b) = v.as_bool() {
        out.push(2);
        out.push(b as u8);
    } else if let Some(n) = v.as_number() {
        out.push(3);
        out.extend_from_slice(&n.to_bits().to_be_bytes());
    } else if let Some(i) = v.as_int() {
        out.push(4);
        out.extend_from_slice(&i.to_be_bytes());
    } else if let Some(s) = v.as_str() {
        out.push(5);
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    } else if let Some(id) = v.as_symbol() {
        out.push(6);
        out.extend_from_slice(&id.to_be_bytes());
    } else if let Some(f) = v.as_function() {
        if allow_fn && f.program == program_id {
            out.push(9);
            out.extend_from_slice(&(f.ptr as u32).to_be_bytes());
            out.extend_from_slice(&(f.cells.len() as u32).to_be_bytes());
            for c in &f.cells {
                write_spawn_value(out, &c.borrow(), true, program_id);
            }
        } else {
            out.push(0);
        }
    } else if let Some(arr) = v.as_array() {
        out.push(7);
        let arr = arr.borrow();
        out.extend_from_slice(&(arr.len() as u32).to_be_bytes());
        for e in arr.to_values() {
            write_spawn_value(out, &e, allow_fn, program_id);
        }
    } else if let Some(m) = v.as_object() {
        out.push(8);
        let m = m.borrow();
        out.extend_from_slice(&(m.len() as u32).to_be_bytes());
        for (k, val) in m.iter_sorted() {
            out.extend_from_slice(&(k.len() as u32).to_be_bytes());
            out.extend_from_slice(k.as_bytes());
            write_spawn_value(out, val, allow_fn, program_id);
        }
    } else {
        out.push(0);
    }
}

pub fn decode_spawn_value(bytes: &[u8], pos: &mut usize) -> Value {
    decode_spawn_value_depth(bytes, pos, 0)
}

fn decode_spawn_value_depth(bytes: &[u8], pos: &mut usize, depth: usize) -> Value {
    if depth > 64 || *pos >= bytes.len() {
        return Value::undefined();
    }
    let tag = bytes[*pos];
    *pos += 1;
    let read_u32 = |bytes: &[u8], p: &mut usize| -> u32 {
        if *p + 4 > bytes.len() {
            *p = bytes.len();
            return 0;
        }
        let b = &bytes[*p..*p + 4];
        *p += 4;
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    };
    let read_i64 = |bytes: &[u8], p: &mut usize| -> i64 {
        if *p + 8 > bytes.len() {
            *p = bytes.len();
            return 0;
        }
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[*p..*p + 8]);
        *p += 8;
        i64::from_be_bytes(raw)
    };
    match tag {
        0 => Value::undefined(),
        1 => Value::null(),
        2 => {
            if *pos >= bytes.len() {
                return Value::bool(false);
            }
            let b = bytes[*pos] != 0;
            *pos += 1;
            Value::bool(b)
        }
        3 => {
            let bits = read_i64(bytes, pos) as u64;
            Value::number(f64::from_bits(bits))
        }
        4 => Value::int(read_i64(bytes, pos)),
        5 => {
            let len = read_u32(bytes, pos) as usize;
            let end = (*pos + len).min(bytes.len());
            let s = String::from_utf8_lossy(&bytes[*pos..end]).to_string();
            *pos = end;
            Value::string(s)
        }
        6 => Value::symbol(read_i64(bytes, pos) as u64),
        7 => {
            let n = (read_u32(bytes, pos) as usize).min(100_000);
            let mut arr = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                if *pos >= bytes.len() {
                    break;
                }
                arr.push(decode_spawn_value_depth(bytes, pos, depth + 1));
            }
            Value::array(arr)
        }
        8 => {
            let n = (read_u32(bytes, pos) as usize).min(100_000);
            let mut m =
                hashbrown::HashMap::with_capacity_and_hasher(n.min(1024), Default::default());
            for _ in 0..n {
                if *pos >= bytes.len() {
                    break;
                }
                let len = read_u32(bytes, pos) as usize;
                let end = (*pos + len).min(bytes.len());
                let k = String::from_utf8_lossy(&bytes[*pos..end]).to_string();
                *pos = end;
                let v = decode_spawn_value_depth(bytes, pos, depth + 1);
                m.insert(k, v);
            }
            Value::object(m)
        }
        9 => {
            let entry = read_u32(bytes, pos) as usize;
            let n = (read_u32(bytes, pos) as usize).min(100_000);
            let mut cells = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                if *pos >= bytes.len() {
                    break;
                }
                cells.push(Rc::new(RefCell::new(decode_spawn_value_depth(
                    bytes,
                    pos,
                    depth + 1,
                ))));
            }
            Value::function(FunctionData {
                program: 0,
                ptr: entry,
                params: 0,
                uses_args: 0,
                is_generator: false,
                cells,
                props: RefCell::new(None),
            })
        }
        _ => Value::undefined(),
    }
}

pub(crate) fn spawn_worker_thread(
    payload: Vec<u8>,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    id: u64,
    registry: Arc<SharedModuleRegistry>,
    py_registry: Arc<SharedPyRegistry>,
    dir: Option<std::path::PathBuf>,
) {
    let tx2 = tx.clone();
    let result = std::panic::catch_unwind(move || {
        spawn_worker_inner(payload, tx, id, registry, py_registry, dir)
    });
    if result.is_err() {
        let _ = tx2.send((id, vec![1]));
    }
}

pub(crate) fn spawn_worker_inner(
    payload: Vec<u8>,
    tx: mpsc::Sender<(u64, Vec<u8>)>,
    id: u64,
    registry: Arc<SharedModuleRegistry>,
    py_registry: Arc<SharedPyRegistry>,
    dir: Option<std::path::PathBuf>,
) {
    let read_u32 = |bytes: &[u8], p: &mut usize| -> u32 {
        let b = &bytes[*p..*p + 4];
        *p += 4;
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    };
    let mut pos = 0usize;
    let plen = read_u32(&payload, &mut pos) as usize;
    let program_bytes = &payload[pos..pos + plen];
    pos += plen;
    let entry = read_u32(&payload, &mut pos) as usize;
    let n = read_u32(&payload, &mut pos) as usize;
    let mut upvalues = Vec::with_capacity(n);
    for _ in 0..n {
        upvalues.push(decode_spawn_value(&payload, &mut pos));
    }
    let nargs = read_u32(&payload, &mut pos) as usize;
    let mut args = Vec::with_capacity(nargs);
    for _ in 0..nargs {
        args.push(decode_spawn_value(&payload, &mut pos));
    }
    let program = match Program::from_bytes(program_bytes) {
        Ok(p) => p,
        Err(_) => {
            let _ = tx.send((id, vec![1]));
            return;
        }
    };
    let mut vm = Vm::new_worker(program, registry, py_registry, dir);
    let heap_ptr: *mut ArenaHeap = &mut vm.heap;
    let _g = HeapGuard::set(heap_ptr);
    let cells: Vec<Rc<RefCell<Value>>> = upvalues
        .into_iter()
        .map(|v| Rc::new(RefCell::new(v)))
        .collect();
    let f = Value::function(FunctionData {
        program: 0,
        ptr: entry,
        params: 0,
        uses_args: 0,
        is_generator: false,
        cells,
        props: RefCell::new(None),
    });
    let result = vm.call_value(&f, &args);
    let mut out = Vec::new();
    if let Some(err) = vm.take_error() {
        out.push(1);
        write_spawn_value(&mut out, &err, false, 0);
    } else if let Some(p) = result.as_promise() {
        vm.drive_event_loop();
        let status = if let Some(err) = vm.take_error() {
            out.push(1);
            write_spawn_value(&mut out, &err, false, 0);
            None
        } else {
            Some(p.lock().unwrap_or_else(|g| g.into_inner()).status.clone())
        };
        if let Some(status) = status {
            match status {
                PromiseStatus::Fulfilled(v) => {
                    out.push(0);
                    write_spawn_value(&mut out, &v, false, 0);
                }
                PromiseStatus::Rejected(e) => {
                    out.push(1);
                    write_spawn_value(&mut out, &e, false, 0);
                }
                PromiseStatus::Pending => out.push(2),
            }
        }
    } else {
        out.push(0);
        write_spawn_value(&mut out, &result, false, 0);
    }
    let _ = tx.send((id, out));
}
