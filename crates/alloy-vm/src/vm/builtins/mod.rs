pub(crate) mod arrays;
pub(crate) mod containers;
pub(crate) mod date;
pub(crate) mod http_server;
pub(crate) mod json;
pub(crate) mod numbers;
pub(crate) mod path;
pub(crate) mod proxy;
pub(crate) mod strings;
pub(crate) mod symbol;
pub(crate) mod web;

use super::spawn::make_spawn_fn;
use alloy_core::shared_memory::{SharedMemoryError, SidecarMemory};
use alloy_core::value::{to_string_js, PromiseState, PromiseStatus, Value, VmHost};
use hashbrown::HashMap;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use self::arrays::{make_array_module, make_object_module};
use self::containers::{make_channel_module, make_map_ctor, make_set_ctor};
use self::date::make_date_ctor;
use self::http_server::{hex_val, make_http_module, url_decode};
use self::json::make_json_module;
use self::numbers::{js_parse_float, js_parse_int, make_math_module, make_number_module};
use self::path::make_path_module;
use self::proxy::{make_proxy_ctor, make_reflect_module};
use self::strings::make_string_module;
use self::symbol::make_symbol_ctor;
use self::web::{
    base64_decode, base64_encode, encode_uri_component, make_crypto_module, make_fetch_sync,
    make_url_ctor,
};

pub(crate) fn make_print_fn(sink: Option<Arc<Mutex<Vec<String>>>>) -> Value {
    Value::native(Arc::new(move |args, _vm| {
        let parts: Vec<String> = args
            .iter()
            .map(|arg| match arg.as_str() {
                Some(s) => s.to_string(),
                None => format!("{}", arg),
            })
            .collect();
        let line = parts.join(" ");
        match &sink {
            Some(s) => {
                s.lock().unwrap().push(line);
            }
            None => println!("{}", line),
        }
        Value::undefined()
    }))
}

/// The zero-copy polyglot bridge: typed-array allocation and scalar read/write
/// over one shared segment. A sidecar process (Python via ctypes, another
/// binary) that knows the segment's base address reads exactly the bytes the
/// JS runtime writes — no serialization, no copy.
pub(crate) fn make_memory_module(shared: Arc<SidecarMemory>) -> Value {
    let base = shared.raw_ptr() as usize;
    // offset_of: turn a JS-visible `buf.ptr` back into a segment offset.
    let offset_of = move |p: f64| -> Option<usize> {
        let p = p as usize;
        if p >= base {
            Some(p - base)
        } else {
            None
        }
    };

    let make_alloc = |shared: &Arc<SidecarMemory>,
                      f: fn(&SidecarMemory, &[f32]) -> Result<usize, SharedMemoryError>|
     -> Value {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<f32> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number() as f32)
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as f32]
            } else if let Some(n) = args.first().and_then(|v| v.as_number()) {
                vec![n as f32]
            } else {
                Vec::new()
            };
            match f(&shared, &vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let allocate_f32 = make_alloc(&shared, SidecarMemory::allocate_float32_array);
    let allocate_f64 = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<f64> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number())
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_number()) {
                vec![n]
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as f64]
            } else {
                Vec::new()
            };
            match shared.allocate_float64_array(&vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let allocate_i32 = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let vals: Vec<i32> = if let Some(items) = args.first().and_then(|v| v.as_array()) {
                items
                    .borrow()
                    .to_values()
                    .iter()
                    .map(|v| v.to_number() as i32)
                    .collect()
            } else if let Some(n) = args.first().and_then(|v| v.as_int()) {
                vec![n as i32]
            } else {
                Vec::new()
            };
            match shared.allocate_int32_array(&vals) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, vals.len())
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };
    let alloc_bytes = {
        let shared = shared.clone();
        Value::native(Arc::new(move |args, _vm| {
            let n = args.first().map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
            match shared.bump(n) {
                Ok(offset) => {
                    let ptr = unsafe { shared.raw_ptr().add(offset) };
                    Value::buffer(ptr, n)
                }
                Err(e) => {
                    eprintln!("alloy shared memory error: {}", e);
                    Value::undefined()
                }
            }
        }))
    };

    // Scalar read/write: `(buf.ptr, element_index_or_byte_offset)`.
    let make_scalar =
        |shared: Arc<SidecarMemory>,
         read: fn(&SidecarMemory, usize) -> Result<f64, SharedMemoryError>,
         write: fn(&SidecarMemory, usize, f64) -> Result<(), SharedMemoryError>| {
            let read = Value::native({
                let shared = shared.clone();
                Arc::new(move |args, _vm| {
                    let p = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                    let off = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
                    match offset_of(p).and_then(|o| read(&shared, o + off).ok()) {
                        Some(v) => Value::number(v),
                        None => Value::undefined(),
                    }
                })
            });
            let write = Value::native({
                let shared = shared.clone();
                Arc::new(move |args, _vm| {
                    let p = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                    let off = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0) as usize;
                    let v = args.get(2).map(|x| x.to_number()).unwrap_or(f64::NAN);
                    let ok = match offset_of(p) {
                        Some(o) => write(&shared, o + off, v).is_ok(),
                        None => false,
                    };
                    Value::bool(ok)
                })
            });
            (read, write)
        };
    let (read_f32, write_f32) = make_scalar(
        shared.clone(),
        |m, o| m.read_float32(o).map(|v| v as f64),
        |m, o, v| m.write_float32(o, v as f32),
    );
    let (read_f64, write_f64) = make_scalar(
        shared.clone(),
        |m, o| m.read_float64(o),
        |m, o, v| m.write_float64(o, v),
    );
    let (read_i32, write_i32) = make_scalar(
        shared.clone(),
        |m, o| m.read_int32(o).map(|v| v as f64),
        |m, o, v| m.write_int32(o, v as i32),
    );
    let (read_u8, write_u8) = make_scalar(
        shared.clone(),
        |m, o| m.read_uint8(o).map(|v| v as f64),
        |m, o, v| m.write_uint8(o, v as u8),
    );

    let size = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| {
            Value::int(shared.capacity() as i64)
        }))
    };
    let used = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| Value::int(shared.used() as i64)))
    };
    let available = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| {
            Value::int(shared.available() as i64)
        }))
    };
    let reset = {
        let shared = shared.clone();
        Value::native(Arc::new(move |_args, _vm| {
            shared.reset();
            Value::undefined()
        }))
    };

    let mut m = HashMap::new();
    m.insert("allocateFloat32Array".to_string(), allocate_f32);
    m.insert("allocateFloat64Array".to_string(), allocate_f64);
    m.insert("allocateInt32Array".to_string(), allocate_i32);
    m.insert("allocBytes".to_string(), alloc_bytes);
    m.insert("readFloat32".to_string(), read_f32);
    m.insert("writeFloat32".to_string(), write_f32);
    m.insert("readFloat64".to_string(), read_f64);
    m.insert("writeFloat64".to_string(), write_f64);
    m.insert("readInt32".to_string(), read_i32);
    m.insert("writeInt32".to_string(), write_i32);
    m.insert("readUint8".to_string(), read_u8);
    m.insert("writeUint8".to_string(), write_u8);
    m.insert("size".to_string(), size);
    m.insert("used".to_string(), used);
    m.insert("available".to_string(), available);
    m.insert("reset".to_string(), reset);
    Value::object(m)
}

/// All seven standard error constructors, Error first (the others chain
/// their prototypes to Error.prototype so `e instanceof Error` holds). Built
/// fresh per call — each is a `Value` allocated in the caller's heap.
pub(crate) fn error_ctor_map() -> hashbrown::HashMap<String, Value> {
    let error = make_error_ctor("Error", Value::undefined());
    let ep = error.as_native_proto().unwrap_or(Value::undefined());
    let mut m = HashMap::new();
    m.insert("Error".to_string(), error);
    for name in [
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
    ] {
        m.insert(name.to_string(), make_error_ctor(name, ep.clone()));
    }
    m
}

/// One standard error constructor. Instances are objects marked
/// `container = 3` with own `name`, `message`, and `stack` properties and a
/// prototype (chained to `parent` — Error.prototype for subclasses) carrying
/// `toString`. Both `new TypeError("x")` and `TypeError("x")` build the
/// instance; `e instanceof TypeError` walks the proto chain; and display /
/// string coercion shows `TypeError: x` (Node's shape).
pub(crate) fn make_error_ctor(name: &str, parent: Value) -> Value {
    let name_owned = name.to_string();
    let proto = Value::object_with_proto(parent);
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        od.set("name", Value::string(name_owned.clone()));
        od.set("message", Value::string(String::new()));
    }
    // `Error.prototype.toString()`: "Name: message" (just "Name" when the
    // message is empty).
    let proto_name = name_owned.clone();
    let to_string = Value::native(Arc::new(move |_args, vm| {
        let this = vm.this_value();
        let (n, m) = match this.as_object() {
            Some(od) => {
                let od = od.borrow();
                let n = od
                    .get("name")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| proto_name.clone());
                let m = od
                    .get("message")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                (n, m)
            }
            None => (proto_name.clone(), String::new()),
        };
        Value::string(if m.is_empty() {
            n
        } else {
            format!("{}: {}", n, m)
        })
    }));
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        od.set("toString", to_string);
    }
    let proto_for_ctor = proto.clone();
    let ctor_name = name_owned.clone();
    let ctor = Arc::new(move |args: &[Value], vm: &mut dyn VmHost| {
        let msg = match args.first() {
            Some(v) if v.is_undefined() => String::new(),
            Some(v) => to_string_js(v),
            None => String::new(),
        };
        let stack_str = vm.format_stack_trace(&ctor_name, &msg);
        let obj = Value::object_with_proto(proto_for_ctor.clone());
        {
            let od = obj.as_object().unwrap();
            let mut od = od.borrow_mut();
            od.container = 3;
            od.set("name", Value::string(ctor_name.clone()));
            od.set("message", Value::string(msg));
            od.set("stack", Value::string(stack_str));
        }
        obj
    });
    let ctor_val = Value::native_ctor(ctor, proto);
    if name == "Error" {
        let capture_stack_trace = Value::native(Arc::new(|args, vm| {
            if let Some(target) = args.first() {
                let constructor_opt = args.get(1);
                vm.capture_stack_trace(target, constructor_opt);
            }
            Value::undefined()
        }));
        if let Some(props) = ctor_val.as_native_props() {
            let mut slot = props.borrow_mut();
            let map = slot.get_or_insert_with(|| Rc::new(RefCell::new(hashbrown::HashMap::new())));
            map.borrow_mut()
                .insert("captureStackTrace".to_string(), capture_stack_trace);
        }
    }
    ctor_val
}

/// Seed the value of a global by name (natives for the builtins, undefined
/// for user globals; REPL lines carry values over by name).
pub(crate) fn seed_global(
    name: &str,
    output: Option<Arc<Mutex<Vec<String>>>>,
    shared: Arc<SidecarMemory>,
) -> Value {
    match name {
        "fetchSync" => make_fetch_sync(),
        "crypto" => make_crypto_module(),
        "URL" => make_url_ctor(),
        "encodeURIComponent" => Value::native(Arc::new(|args, _vm| {
            let s = args
                .first()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            Value::string(encode_uri_component(&s))
        })),
        "decodeURIComponent" => Value::native(Arc::new(|args, vm| {
            let s = args
                .first()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            // Strict like V8: a bare `%` or bad hex is a URIError, not silent.
            let mut ok = true;
            let b = s.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' {
                    if i + 2 >= b.len()
                        || hex_val(b[i + 1]).is_none()
                        || hex_val(b[i + 2]).is_none()
                    {
                        ok = false;
                        break;
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            if !ok {
                vm.throw_exception(Value::string(
                    "URIError: malformed URI sequence".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(url_decode(&s))
        })),
        "encodeURI" => Value::native(Arc::new(|args, _vm| {
            // encodeURI leaves `;/?:@&=+$,#` (valid URI punctuation) alone.
            let s = args
                .first()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            let mut out = String::with_capacity(s.len());
            for b in s.as_bytes() {
                match b {
                    b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'\''
                    | b'('
                    | b')'
                    | b';'
                    | b','
                    | b'/'
                    | b'?'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b'#' => out.push(*b as char),
                    _ => out.push_str(&format!("%{:02X}", b)),
                }
            }
            Value::string(out)
        })),
        "decodeURI" => Value::native(Arc::new(|args, _vm| {
            Value::string(url_decode(
                &args
                    .first()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .unwrap_or_default(),
            ))
        })),
        "btoa" => Value::native(Arc::new(|args, vm| {
            let s = args
                .first()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            if !s.is_ascii() {
                vm.throw_exception(Value::string(
                    "Error: btoa input must be Latin-1".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(base64_encode(s.as_bytes()))
        })),
        "atob" => Value::native(Arc::new(|args, vm| {
            let s = args
                .first()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            match base64_decode(&s) {
                Some(b) => Value::string(String::from_utf8_lossy(&b).into_owned()),
                None => {
                    vm.throw_exception(Value::string("Error: invalid base64".to_string()));
                    Value::undefined()
                }
            }
        })),
        "print" => make_print_fn(output),
        "http" => make_http_module(),
        "memory" => make_memory_module(shared),
        "fs" => make_fs_module(),
        "path" => make_path_module(),
        "process" => make_process_module(),
        "Promise" => make_promise_module(),
        "setTimeout" => make_set_timeout(),
        "setInterval" => make_set_interval(),
        "clearTimeout" => make_clear_timer(),
        "clearInterval" => make_clear_timer(),
        "queueMicrotask" => make_queue_microtask(),
        "console" => make_console(output),
        "Array" => make_array_module(),
        "String" => make_string_module(),
        "channel" => make_channel_module(),
        "spawn" => make_spawn_fn(),
        "require" => Value::native(Arc::new(|args, vm| {
            let path = match args.first() {
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                Some(v) => {
                    vm.throw_exception(Value::string(format!(
                        "TypeError: require() expects a string path, got {}",
                        v.type_name()
                    )));
                    return Value::undefined();
                }
                None => {
                    vm.throw_exception(Value::string(
                        "TypeError: require() expects a path".to_string(),
                    ));
                    return Value::undefined();
                }
            };
            vm.require_module(&path)
        })),
        "reload" => Value::native(Arc::new(|args, vm| {
            let path = match args.first() {
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                _ => return Value::bool(false),
            };
            Value::bool(vm.reload_module(&path))
        })),
        "__alloy_import" => Value::native(Arc::new(|args, vm| {
            let path = match args.first() {
                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                Some(v) => alloy_core::value::to_string_js(v),
                None => String::new(),
            };
            let p = vm.new_promise();
            let mod_val = vm.require_module(&path);
            if let Some(err) = vm.take_uncaught_exception() {
                vm.reject_promise(&p, err);
            } else {
                vm.resolve_promise(&p, mod_val);
            }
            p
        })),
        "Date" => make_date_ctor(),
        "Math" => make_math_module(),
        "Map" => make_map_ctor(),
        "Set" => make_set_ctor(),
        "Symbol" => make_symbol_ctor(),
        "Proxy" => make_proxy_ctor(),
        "Reflect" => make_reflect_module(),
        "globalThis" => make_global_this(output.clone(), shared.clone()),
        "structuredClone" => make_structured_clone(),
        "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError" | "EvalError"
        | "URIError" => error_ctor_map().remove(name).unwrap_or(Value::undefined()),
        "JSON" => make_json_module(),
        "Number" => make_number_module(),
        "Object" => make_object_module(),
        "parseInt" => Value::native(Arc::new(|args, _vm| {
            js_parse_int(
                &args.first().cloned().unwrap_or(Value::undefined()),
                &args.get(1).cloned().unwrap_or(Value::undefined()),
            )
        })),
        "parseFloat" => Value::native(Arc::new(|args, _vm| {
            js_parse_float(&args.first().cloned().unwrap_or(Value::undefined()))
        })),
        "isNaN" => Value::native(Arc::new(|args, _vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(v.to_number().is_nan())
        })),
        // `sweepSegments()` — reclaim orphaned `alloy_shm_*.tmp` segment
        // files left by crashed runs, without waiting for the automatic
        // sweep's 60s rate-limit window. Safe to call freely (a live
        // process's segment is never touched; young files survive the grace
        // period). Returns `{ files, bytes }` for what was reclaimed — a
        // long-running server can call this between requests and log or
        // alert on `files > 0` to spot leaks from long-ago crashes.
        "sweepSegments" => Value::native(Arc::new(|_args, _vm| {
            let s = alloy_core::shared_memory::sweep_segments_now();
            let mut m = HashMap::new();
            m.insert("files".to_string(), Value::number(s.files as f64));
            m.insert("bytes".to_string(), Value::number(s.bytes as f64));
            Value::object(m)
        })),
        // `finalizePythonEmbed()` — optional clean teardown of the
        // in-process CPython interpreter (ALLOY_PYTHON_EMBED=1 only). Tears
        // down this VM's python pools and calls Py_FinalizeEx; terminal, so
        // a server can shut python down cleanly at exit. After it succeeds,
        // embed mode is off for the process and later `.py` imports fall
        // back to child sidecars. Returns `{ finalized, error,
        // liveBackends }`.
        "finalizePythonEmbed" => Value::native(Arc::new(|_args, vm| vm.finalize_python_embed())),
        "NaN" => Value::number(f64::NAN),
        "Infinity" => Value::number(f64::INFINITY),
        "module" => {
            let mut m = HashMap::new();
            m.insert("exports".to_string(), Value::object(HashMap::new()));
            Value::object(m)
        }
        "exports" => Value::object(HashMap::new()),
        _ => Value::undefined(),
    }
}

/// True for the seven standard error constructor names. Used both by the
/// constructor's group-seeding path and by `seed_global_named`.
pub(crate) fn is_error_name(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "ReferenceError"
            | "SyntaxError"
            | "EvalError"
            | "URIError"
    )
}

pub(crate) fn make_promise_module() -> Value {
    let resolve = Value::native(Arc::new(|args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        // Stamped with the creating VM's wake handle so a settlement routed
        // from another thread reaches the right event loop.
        let wake = vm.wake_handle();
        Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Fulfilled(v),
            continuations: Vec::new(),
            owner: wake,
        })))
    }));
    let with_resolvers = Value::native(Arc::new(|_args, vm| {
        let p = vm.new_promise();
        let p_arc = p.as_promise().expect("new_promise").clone();
        let p_resolve = p_arc.clone();
        let p_reject = p_arc.clone();
        let resolve = Value::native(Arc::new(move |args, vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            vm.resolve_promise(&Value::promise(p_resolve.clone()), v);
            Value::undefined()
        }));
        let reject = Value::native(Arc::new(move |args, vm| {
            let v = args.first().cloned().unwrap_or(Value::undefined());
            vm.reject_promise(&Value::promise(p_reject.clone()), v);
            Value::undefined()
        }));
        let mut obj = HashMap::new();
        obj.insert("promise".to_string(), p);
        obj.insert("resolve".to_string(), resolve);
        obj.insert("reject".to_string(), reject);
        Value::object(obj)
    }));
    let reject_fn = Value::native(Arc::new(|args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        let wake = vm.wake_handle();
        Value::promise(Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Rejected(v),
            continuations: Vec::new(),
            owner: wake,
        })))
    }));
    let all = Value::native(Arc::new(|args, vm| {
        let input = args.first().cloned().unwrap_or(Value::undefined());
        let items: Vec<Value> = if let Some(arr) = input.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        if items.is_empty() {
            return Value::promise(Arc::new(Mutex::new(PromiseState {
                status: PromiseStatus::Fulfilled(Value::array_empty()),
                continuations: Vec::new(),
                owner: vm.wake_handle(),
            })));
        }
        let count = items.len();
        let remaining = Arc::new(Mutex::new(count));
        let results = Arc::new(Mutex::new(vec![Value::undefined(); count]));
        let out_p = vm.new_promise();
        let out_arc = out_p.as_promise().unwrap().clone();
        for (i, item) in items.into_iter().enumerate() {
            let rem = remaining.clone();
            let res = results.clone();
            let p_arc = out_arc.clone();
            let on_fulfill = Value::native(Arc::new(move |args, vm| {
                let v = args.first().cloned().unwrap_or(Value::undefined());
                res.lock().unwrap()[i] = v;
                let mut r = rem.lock().unwrap();
                *r -= 1;
                if *r == 0 {
                    let final_vals = res.lock().unwrap().clone();
                    vm.resolve_promise(&Value::promise(p_arc.clone()), Value::array(final_vals));
                }
                Value::undefined()
            }));
            let p_arc_fail = out_arc.clone();
            let on_reject = Value::native(Arc::new(move |args, vm| {
                let err = args.first().cloned().unwrap_or(Value::undefined());
                vm.reject_promise(&Value::promise(p_arc_fail.clone()), err);
                Value::undefined()
            }));
            if item.is_promise() {
                vm.then(&item, on_fulfill, Some(on_reject));
            } else {
                let mut r = remaining.lock().unwrap();
                results.lock().unwrap()[i] = item;
                *r -= 1;
                if *r == 0 {
                    let final_vals = results.lock().unwrap().clone();
                    return Value::promise(Arc::new(Mutex::new(PromiseState {
                        status: PromiseStatus::Fulfilled(Value::array(final_vals)),
                        continuations: Vec::new(),
                        owner: vm.wake_handle(),
                    })));
                }
            }
        }
        out_p
    }));
    let all_settled = Value::native(Arc::new(|args, vm| {
        let input = args.first().cloned().unwrap_or(Value::undefined());
        let items: Vec<Value> = if let Some(arr) = input.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        if items.is_empty() {
            return Value::promise(Arc::new(Mutex::new(PromiseState {
                status: PromiseStatus::Fulfilled(Value::array_empty()),
                continuations: Vec::new(),
                owner: vm.wake_handle(),
            })));
        }
        let count = items.len();
        let remaining = Arc::new(Mutex::new(count));
        let results = Arc::new(Mutex::new(vec![Value::undefined(); count]));
        let out_p = vm.new_promise();
        let out_arc = out_p.as_promise().unwrap().clone();
        for (i, item) in items.into_iter().enumerate() {
            let rem = remaining.clone();
            let res = results.clone();
            let p_arc = out_arc.clone();
            let on_fulfill = Value::native(Arc::new(move |args, vm| {
                let v = args.first().cloned().unwrap_or(Value::undefined());
                let mut m = HashMap::new();
                m.insert("status".to_string(), Value::string("fulfilled".to_string()));
                m.insert("value".to_string(), v);
                res.lock().unwrap()[i] = Value::object(m);
                let mut r = rem.lock().unwrap();
                *r -= 1;
                if *r == 0 {
                    let final_vals = res.lock().unwrap().clone();
                    vm.resolve_promise(&Value::promise(p_arc.clone()), Value::array(final_vals));
                }
                Value::undefined()
            }));
            let rem2 = remaining.clone();
            let res2 = results.clone();
            let p_arc2 = out_arc.clone();
            let on_reject = Value::native(Arc::new(move |args, vm| {
                let err = args.first().cloned().unwrap_or(Value::undefined());
                let mut m = HashMap::new();
                m.insert("status".to_string(), Value::string("rejected".to_string()));
                m.insert("reason".to_string(), err);
                res2.lock().unwrap()[i] = Value::object(m);
                let mut r = rem2.lock().unwrap();
                *r -= 1;
                if *r == 0 {
                    let final_vals = res2.lock().unwrap().clone();
                    vm.resolve_promise(&Value::promise(p_arc2.clone()), Value::array(final_vals));
                }
                Value::undefined()
            }));
            if item.is_promise() {
                vm.then(&item, on_fulfill, Some(on_reject));
            } else {
                let mut m = HashMap::new();
                m.insert("status".to_string(), Value::string("fulfilled".to_string()));
                m.insert("value".to_string(), item);
                results.lock().unwrap()[i] = Value::object(m);
                let mut r = remaining.lock().unwrap();
                *r -= 1;
                if *r == 0 {
                    let final_vals = results.lock().unwrap().clone();
                    return Value::promise(Arc::new(Mutex::new(PromiseState {
                        status: PromiseStatus::Fulfilled(Value::array(final_vals)),
                        continuations: Vec::new(),
                        owner: vm.wake_handle(),
                    })));
                }
            }
        }
        out_p
    }));
    let any_fn = Value::native(Arc::new(|args, vm| {
        let input = args.first().cloned().unwrap_or(Value::undefined());
        let items: Vec<Value> = if let Some(arr) = input.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        if items.is_empty() {
            return Value::promise(Arc::new(Mutex::new(PromiseState {
                status: PromiseStatus::Rejected(Value::string(
                    "AggregateError: All promises were rejected".to_string(),
                )),
                continuations: Vec::new(),
                owner: vm.wake_handle(),
            })));
        }
        let count = items.len();
        let remaining = Arc::new(Mutex::new(count));
        let errors = Arc::new(Mutex::new(vec![Value::undefined(); count]));
        let settled = Arc::new(Mutex::new(false));
        let out_p = vm.new_promise();
        let out_arc = out_p.as_promise().unwrap().clone();
        for (i, item) in items.into_iter().enumerate() {
            let set1 = settled.clone();
            let p_arc1 = out_arc.clone();
            let on_fulfill = Value::native(Arc::new(move |args, vm| {
                let mut s = set1.lock().unwrap();
                if !*s {
                    *s = true;
                    let v = args.first().cloned().unwrap_or(Value::undefined());
                    vm.resolve_promise(&Value::promise(p_arc1.clone()), v);
                }
                Value::undefined()
            }));
            let set2 = settled.clone();
            let p_arc2 = out_arc.clone();
            let rem2 = remaining.clone();
            let errs2 = errors.clone();
            let on_reject = Value::native(Arc::new(move |args, vm| {
                let err = args.first().cloned().unwrap_or(Value::undefined());
                errs2.lock().unwrap()[i] = err;
                let mut r = rem2.lock().unwrap();
                *r -= 1;
                if *r == 0 {
                    let mut s = set2.lock().unwrap();
                    if !*s {
                        *s = true;
                        vm.reject_promise(
                            &Value::promise(p_arc2.clone()),
                            Value::string("AggregateError: All promises were rejected".to_string()),
                        );
                    }
                }
                Value::undefined()
            }));
            if item.is_promise() {
                vm.then(&item, on_fulfill, Some(on_reject));
            } else {
                return Value::promise(Arc::new(Mutex::new(PromiseState {
                    status: PromiseStatus::Fulfilled(item),
                    continuations: Vec::new(),
                    owner: vm.wake_handle(),
                })));
            }
        }
        out_p
    }));
    let race_fn = Value::native(Arc::new(|args, vm| {
        let input = args.first().cloned().unwrap_or(Value::undefined());
        let items: Vec<Value> = if let Some(arr) = input.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        let settled = Arc::new(Mutex::new(false));
        let out_p = vm.new_promise();
        let out_arc = out_p.as_promise().unwrap().clone();
        for item in items {
            let set1 = settled.clone();
            let p_arc1 = out_arc.clone();
            let on_fulfill = Value::native(Arc::new(move |args, vm| {
                let mut s = set1.lock().unwrap();
                if !*s {
                    *s = true;
                    let v = args.first().cloned().unwrap_or(Value::undefined());
                    vm.resolve_promise(&Value::promise(p_arc1.clone()), v);
                }
                Value::undefined()
            }));
            let set2 = settled.clone();
            let p_arc2 = out_arc.clone();
            let on_reject = Value::native(Arc::new(move |args, vm| {
                let mut s = set2.lock().unwrap();
                if !*s {
                    *s = true;
                    let err = args.first().cloned().unwrap_or(Value::undefined());
                    vm.reject_promise(&Value::promise(p_arc2.clone()), err);
                }
                Value::undefined()
            }));
            if item.is_promise() {
                vm.then(&item, on_fulfill, Some(on_reject));
            } else {
                return Value::promise(Arc::new(Mutex::new(PromiseState {
                    status: PromiseStatus::Fulfilled(item),
                    continuations: Vec::new(),
                    owner: vm.wake_handle(),
                })));
            }
        }
        out_p
    }));
    let mut m = HashMap::new();
    m.insert("resolve".to_string(), resolve);
    m.insert("reject".to_string(), reject_fn);
    m.insert("all".to_string(), all);
    m.insert("allSettled".to_string(), all_settled);
    m.insert("any".to_string(), any_fn);
    m.insert("race".to_string(), race_fn);
    m.insert("withResolvers".to_string(), with_resolvers);
    Value::object(m)
}

pub(crate) fn structured_clone_val(v: &Value, vm: &mut dyn VmHost) -> Value {
    if v.is_undefined()
        || v.is_null()
        || v.as_bool().is_some()
        || v.is_number()
        || v.as_int().is_some()
        || v.is_string()
    {
        return v.clone();
    }
    if let Some(arr) = v.as_array() {
        let items: Vec<Value> = arr
            .borrow()
            .to_values()
            .iter()
            .map(|item| structured_clone_val(item, vm))
            .collect();
        return Value::array(items);
    }
    if let Some(od) = v.as_object() {
        let od_b = od.borrow();
        if od_b.container == alloy_core::value::DATE_CONTAINER {
            if let Some(ms) = alloy_core::value::date_ms(v) {
                let d = Value::map(Value::undefined(), alloy_core::value::DATE_CONTAINER);
                alloy_core::value::date_set_ms(&d, ms);
                return d;
            }
        }
        if od_b.container == 1 {
            let m = Value::map(Value::undefined(), 1);
            for (k, val) in self::containers::container_pairs(v) {
                let k_cloned = structured_clone_val(&k, vm);
                let v_cloned = structured_clone_val(&val, vm);
                self::containers::container_insert(&m, k_cloned, v_cloned, vm);
            }
            return m;
        }
        if od_b.container == 2 {
            let s = Value::map(Value::undefined(), 2);
            for (_, val) in self::containers::container_pairs(v) {
                let v_cloned = structured_clone_val(&val, vm);
                self::containers::container_insert(&s, v_cloned.clone(), v_cloned, vm);
            }
            return s;
        }
        let mut props = HashMap::new();
        for k in od_b.keys_live() {
            if let Some(val) = od_b.get(k) {
                props.insert(k.clone(), structured_clone_val(val, vm));
            }
        }
        return Value::object(props);
    }
    if let Some(r) = v.as_regex() {
        let guard = r.lock().unwrap_or_else(|g| g.into_inner());
        return Value::regex(guard.compiled.clone());
    }
    if v.is_symbol() || v.is_function() || v.is_native() {
        vm.throw_exception(Value::string(
            "DOMException: The object could not be cloned".to_string(),
        ));
        return Value::undefined();
    }
    v.clone()
}

pub(crate) fn make_structured_clone() -> Value {
    Value::native(Arc::new(|args, vm| {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        structured_clone_val(&v, vm)
    }))
}

pub(crate) fn make_global_this(
    output: Option<Arc<Mutex<Vec<String>>>>,
    shared: Arc<SidecarMemory>,
) -> Value {
    let mut m = HashMap::new();
    let names = [
        "Array",
        "String",
        "Number",
        "Date",
        "Math",
        "JSON",
        "Promise",
        "Map",
        "Set",
        "Symbol",
        "Proxy",
        "Reflect",
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
        "parseInt",
        "parseFloat",
        "isNaN",
        "setTimeout",
        "setInterval",
        "clearTimeout",
        "clearInterval",
        "queueMicrotask",
        "console",
        "structuredClone",
        "btoa",
        "atob",
        "encodeURI",
        "decodeURI",
        "encodeURIComponent",
        "decodeURIComponent",
        "NaN",
        "Infinity",
        "__alloy_import",
    ];
    for name in names {
        m.insert(
            name.to_string(),
            seed_global(name, output.clone(), shared.clone()),
        );
    }
    let gt = Value::object(m);
    if let Some(od) = gt.as_object() {
        od.borrow_mut().set("globalThis", gt.clone());
    }
    gt
}

/// `setTimeout(cb, ms)` — one-shot timer, returns a numeric handle id that
/// `clearTimeout` accepts. `setInterval(cb, ms)` is the repeating variant.
pub(crate) fn make_set_timeout() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        let ms = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0);
        Value::int(vm.schedule_timer(cb, ms, None) as i64)
    }))
}

pub(crate) fn make_set_interval() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        let ms = args.get(1).map(|v| v.to_number()).unwrap_or(0.0).max(0.0);
        Value::int(vm.schedule_timer(cb, ms, Some(ms)) as i64)
    }))
}

/// `clearTimeout(id)` / `clearInterval(id)`: cancel a pending timer by its
/// handle. Accepts any value; non-numeric ids are ignored.
pub(crate) fn make_clear_timer() -> Value {
    Value::native(Arc::new(|args, vm| {
        if let Some(v) = args.first() {
            let n = v.to_number();
            if n.is_finite() && n >= 0.0 {
                vm.clear_timer(n as u64);
            }
        }
        Value::undefined()
    }))
}

/// `queueMicrotask(cb)`: schedule `cb` to run as soon as the current
/// synchronous execution finishes, before any timers. Implemented on the
/// promise continuation machinery: an anonymous promise whose Callback
/// continuation is enqueued immediately.
pub(crate) fn make_queue_microtask() -> Value {
    Value::native(Arc::new(|args, vm| {
        let cb = args.first().cloned().unwrap_or(Value::undefined());
        vm.queue_microtask(cb);
        Value::undefined()
    }))
}

/// `console` global: log/info/warn/error/debug write to stdout (via the print
/// sink, so tests capture them); the others mirror Node's shapes.
pub(crate) fn make_console(sink: Option<Arc<Mutex<Vec<String>>>>) -> Value {
    let make = |label: Option<&'static str>| {
        let sink = sink.clone();
        Value::native(Arc::new(move |args, _vm| {
            let parts: Vec<String> = args
                .iter()
                .map(|arg| match arg.as_str() {
                    Some(s) => s.to_string(),
                    None => format!("{}", arg),
                })
                .collect();
            let line = parts.join(" ");
            let line = match label {
                Some(l) => format!("{} {}", l, line),
                None => line,
            };
            match &sink {
                Some(s) => {
                    s.lock().unwrap().push(line);
                }
                None => println!("{}", line),
            }
            Value::undefined()
        }))
    };
    let mut m = HashMap::new();
    m.insert("log".to_string(), make(None));
    m.insert("info".to_string(), make(None));
    m.insert("debug".to_string(), make(None));
    m.insert("warn".to_string(), make(Some("warn:")));
    m.insert("error".to_string(), make(Some("error:")));
    m.insert("trace".to_string(), make(Some("trace:")));
    m.insert("dir".to_string(), make(None));
    Value::object(m)
}

pub(crate) fn make_fs_module() -> Value {
    let read_file = Value::native(Arc::new(|args, vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                let err = format!(
                    "TypeError: expected string path, got {}",
                    args.first().map(|o| o.to_string()).unwrap_or_default()
                );
                vm.throw_exception(Value::string(err));
                return Value::undefined();
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Value::string(s),
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let write_file = Value::native(Arc::new(|args, vm| {
        let (path, data) = match (args.first().and_then(|v| v.as_str()), args.get(1)) {
            (Some(p), Some(d)) => (p.to_string(), d.clone()),
            _ => {
                vm.throw_exception(Value::string(
                    "TypeError: expected path and data".to_string(),
                ));
                return Value::bool(false);
            }
        };
        let text = match data.as_str() {
            Some(s) => s.to_string(),
            None => format!("{}", data),
        };
        match std::fs::write(&path, text) {
            Ok(_) => Value::bool(true),
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::bool(false)
            }
        }
    }));
    let exists = Value::native(Arc::new(|args, _vm| {
        let ok = matches!(args.first().and_then(|v| v.as_str()), Some(p) if std::path::Path::new(p).exists());
        Value::bool(ok)
    }));
    let unlink = Value::native(Arc::new(|args, vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                vm.throw_exception(Value::string("TypeError: expected string path".to_string()));
                return Value::undefined();
            }
        };
        match std::fs::remove_file(&path) {
            Ok(_) => Value::undefined(),
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let mkdir = Value::native(Arc::new(|args, vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                vm.throw_exception(Value::string("TypeError: expected string path".to_string()));
                return Value::undefined();
            }
        };
        let recursive = args.get(1).and_then(|o| o.as_object()).is_none_or(|obj| {
            obj.borrow()
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        });
        let res = if recursive {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };
        match res {
            Ok(_) => Value::undefined(),
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let readdir = Value::native(Arc::new(|args, vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                vm.throw_exception(Value::string("TypeError: expected string path".to_string()));
                return Value::undefined();
            }
        };
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut names = Vec::new();
                for e in entries.flatten() {
                    names.push(Value::string(e.file_name().to_string_lossy().to_string()));
                }
                Value::array(names)
            }
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let stat = Value::native(Arc::new(|args, vm| {
        let path = match args.first().and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                vm.throw_exception(Value::string("TypeError: expected string path".to_string()));
                return Value::undefined();
            }
        };
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let is_file = meta.is_file();
                let is_dir = meta.is_dir();
                let len = meta.len() as i64;
                let mut sm = HashMap::new();
                sm.insert(
                    "isFile".to_string(),
                    Value::native(Arc::new(move |_, _| Value::bool(is_file))),
                );
                sm.insert(
                    "isDirectory".to_string(),
                    Value::native(Arc::new(move |_, _| Value::bool(is_dir))),
                );
                sm.insert("size".to_string(), Value::int(len));
                Value::object(sm)
            }
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let copy_file = Value::native(Arc::new(|args, vm| {
        let (src, dst) = match (
            args.first().and_then(|v| v.as_str()),
            args.get(1).and_then(|v| v.as_str()),
        ) {
            (Some(s), Some(d)) => (s.to_string(), d.to_string()),
            _ => {
                vm.throw_exception(Value::string(
                    "TypeError: expected src and dst paths".to_string(),
                ));
                return Value::undefined();
            }
        };
        match std::fs::copy(&src, &dst) {
            Ok(_) => Value::undefined(),
            Err(e) => {
                vm.throw_exception(Value::string(format!("Error: {}", e)));
                Value::undefined()
            }
        }
    }));
    let mut m = HashMap::new();
    m.insert("readFileSync".to_string(), read_file);
    m.insert("writeFileSync".to_string(), write_file);
    m.insert("existsSync".to_string(), exists);
    m.insert("unlinkSync".to_string(), unlink);
    m.insert("mkdirSync".to_string(), mkdir);
    m.insert("readdirSync".to_string(), readdir);
    m.insert("statSync".to_string(), stat);
    m.insert("copyFileSync".to_string(), copy_file);
    Value::object(m)
}

pub(crate) fn make_process_module() -> Value {
    let mut proc_map = HashMap::new();
    let mut env_map = HashMap::new();
    for (k, v) in std::env::vars() {
        env_map.insert(k, Value::string(v));
    }
    proc_map.insert("env".to_string(), Value::object(env_map));

    let args: Vec<Value> = std::env::args().map(Value::string).collect();
    proc_map.insert("argv".to_string(), Value::array(args));

    let cwd = Value::native(Arc::new(|_args, _vm| {
        let dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        Value::string(dir)
    }));
    proc_map.insert("cwd".to_string(), cwd);

    let exit = Value::native(Arc::new(|args, _vm| {
        let code = args.first().and_then(|v| v.as_int()).unwrap_or(0) as i32;
        std::process::exit(code);
    }));
    proc_map.insert("exit".to_string(), exit);

    let uptime = Value::native(Arc::new(|_args, _vm| {
        static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let start = START_TIME.get_or_init(std::time::Instant::now);
        Value::number(start.elapsed().as_secs_f64())
    }));
    proc_map.insert("uptime".to_string(), uptime);

    let pid = std::process::id() as i64;
    proc_map.insert("pid".to_string(), Value::int(pid));
    proc_map.insert("version".to_string(), Value::string("v0.2.0".to_string()));
    proc_map.insert(
        "platform".to_string(),
        Value::string(std::env::consts::OS.to_string()),
    );
    proc_map.insert(
        "arch".to_string(),
        Value::string(std::env::consts::ARCH.to_string()),
    );
    Value::object(proc_map)
}
