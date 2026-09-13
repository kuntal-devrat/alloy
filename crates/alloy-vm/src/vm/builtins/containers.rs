use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use hashbrown::HashMap;
use alloy_core::value::{ChannelState, Value, VmHost};
/// A Map/Set *computed* property that cannot live on the prototype as a
/// shared native: `size` must read the instance's table (JS exposes it as a
/// getter, which this engine doesn't model), so it is synthesized per read.
/// Every other method lives once on Map.prototype / Set.prototype and reads
/// its instance from `this` (see [`container_method_native`]).
pub(crate) fn container_prop(obj: &Value, prop: &Value) -> Option<Value> {
    if prop.as_str() == Some("size") {
        // Only Map/Set have a `size`; Error (3) and Date (4) instances read
        // their props through the normal proto walk instead.
        let c = obj
            .as_object()
            .map(|o| o.borrow().container)
            .unwrap_or(0);
        if c == 1 || c == 2 {
            return Some(Value::int(container_len(obj)));
        }
    }
    None
}

/// One shared Map/Set method native, installed once on the prototype and
/// used by every instance: the receiver (`m`, `s`) is read from
/// `VmHost::this_value`, which `dispatch_call` stashes for the duration of
/// the native call. Extracting a method — `let g = m.get; g(k)` — gets
/// `this = undefined`, exactly like Node.
pub(crate) fn container_method_native(name: &str) -> Value {
    match name {
        "get" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            container_get(&m, &k)
        })),
        "set" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            let v = args.get(1).cloned().unwrap_or(Value::undefined());
            container_insert(&m, k, v, vm);
            m
        })),
        "add" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            // Set values are the keys themselves, so `values()`/`entries()`
            // (which read the stored value) yield the element.
            container_insert(&m, k.clone(), k, vm);
            m
        })),
        "has" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(container_contains(&m, &k))
        })),
        "delete" => Value::native(Arc::new(|args, vm| {
            let m = vm.this_value();
            let k = args.first().cloned().unwrap_or(Value::undefined());
            Value::bool(container_remove(&m, &k, vm))
        })),
        "clear" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_clear(&m, vm);
            Value::undefined()
        })),
        "keys" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_keys(&m)
        })),
        "values" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_values(&m)
        })),
        "entries" => Value::native(Arc::new(|_args, vm| {
            let m = vm.this_value();
            container_entries(&m)
        })),
        "forEach" => Value::native(Arc::new(|args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if !(cb.is_function() || cb.is_native()) {
                return Value::undefined();
            }
            let m = vm.this_value();
            // Live walk, not a snapshot: the order length is captured once,
            // but each step re-reads the table, so entries deleted mid-walk
            // are skipped (tombstoned) and entries added during the walk are
            // seen if they land before the walk finishes — Node's spec
            // latitude. A delete-triggered compaction shrinking the order
            // list just ends the walk early.
            let n = {
                let Some(od) = m.as_object() else {
                    return Value::undefined();
                };
                let od = od.borrow();
                od.entries.as_ref().map_or(0, |cd| cd.order_len())
            };
            let mut i = 0usize;
            while i < n {
                let (k, v) = {
                    let Some(od) = m.as_object() else {
                        break;
                    };
                    let od = od.borrow();
                    let Some(cd) = od.entries.as_ref() else {
                        break;
                    };
                    if i >= cd.order_len() {
                        break;
                    }
                    match cd.order_pair(i) {
                        Some(pair) => pair,
                        None => {
                            i += 1;
                            continue;
                        }
                    }
                };
                // Node calls `cb(value, key, map)` — for a Set both value and
                // key are the element, so the same shape serves both.
                vm.call_value(&cb, &[v.clone(), k.clone(), m.clone()]);
                i += 1;
            }
            Value::undefined()
        })),
        _ => unreachable!("unknown container method: {}", name),
    }
}

pub(crate) fn container_len(m: &Value) -> i64 {
    match m.as_object() {
        Some(od) => od.borrow().entries.as_ref().map_or(0, |e| e.len() as i64),
        None => 0,
    }
}

pub(crate) fn container_get(m: &Value, k: &Value) -> Value {
    let Some(od) = m.as_object() else {
        return Value::undefined();
    };
    let od = od.borrow();
    match od.entries.as_ref() {
        Some(e) => e.get(k).cloned().unwrap_or(Value::undefined()),
        None => Value::undefined(),
    }
}

pub(crate) fn container_contains(m: &Value, k: &Value) -> bool {
    let Some(od) = m.as_object() else {
        return false;
    };
    let od = od.borrow();
    od.entries.as_ref().is_some_and(|e| e.contains(k))
}

pub(crate) fn container_insert(m: &Value, k: Value, v: Value, vm: &mut dyn VmHost) {
    let Some(od) = m.as_object() else {
        return;
    };
    // The box may be in the old generation: a young value stored into it
    // must survive the next young collection, so flag it for the dirty scan.
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    od.entries.get_or_insert_with(Default::default).insert(k, v);
}

pub(crate) fn container_remove(m: &Value, k: &Value, vm: &mut dyn VmHost) -> bool {
    let Some(od) = m.as_object() else {
        return false;
    };
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    match od.entries.as_mut() {
        Some(e) => e.remove(k),
        None => false,
    }
}

pub(crate) fn container_clear(m: &Value, vm: &mut dyn VmHost) {
    let Some(od) = m.as_object() else {
        return;
    };
    vm.note_box_dirty(od as *const _ as usize);
    let mut od = od.borrow_mut();
    if let Some(e) = od.entries.as_mut() {
        e.clear();
    }
}

/// Live `(key, value)` pairs in insertion order (values are the elements
/// themselves for Sets).
pub(crate) fn container_pairs(m: &Value) -> Vec<(Value, Value)> {
    let Some(od) = m.as_object() else {
        return Vec::new();
    };
    let od = od.borrow();
    match od.entries.as_ref() {
        Some(cd) => cd.iter().collect(),
        None => Vec::new(),
    }
}

/// `m.keys()` — array snapshot of the keys in insertion order. (The engine
/// has no iterator protocol; the PRD target is the array shape.)
pub(crate) fn container_keys(m: &Value) -> Value {
    let mut out = Vec::new();
    for (k, _) in container_pairs(m) {
        out.push(k);
    }
    Value::array(out)
}

pub(crate) fn container_values(m: &Value) -> Value {
    let mut out = Vec::new();
    for (_, v) in container_pairs(m) {
        out.push(v);
    }
    Value::array(out)
}

pub(crate) fn container_entries(m: &Value) -> Value {
    let mut out = Vec::new();
    for (k, v) in container_pairs(m) {
        out.push(Value::array(vec![k, v]));
    }
    Value::array(out)
}

/// The `Map` constructor: a native that carries `Map.prototype` as its
/// prototype, so `new Map()` builds an instance whose proto chain reaches it
/// (`m instanceof Map`) and `Map.prototype` reads back the same object.
/// Iterable-seed arguments (Node accepts `new Map([[k, v], …])`) are not
/// supported — the ctor ignores its args, matching the PRD's cache-server
/// usage (`m.set(k, v)` after construction).
pub(crate) fn make_map_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    for name in ["get", "set", "has", "delete", "clear", "keys", "values", "entries", "forEach"] {
        if let Some(od) = proto.as_object() {
            od.borrow_mut().set(name, container_method_native(name));
        }
    }
    if let Some(od) = proto.as_object() {
        od.borrow_mut().set("\0sym_1", container_method_native("entries"));
    }
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |args: &[Value], vm: &mut dyn VmHost| {
        let m = Value::map(ctor_proto.clone(), 1);
        if let Some(seed) = args.first() {
            if let Some(arr) = seed.as_array() {
                for item in arr.borrow().to_values() {
                    if let Some(entry) = item.as_array() {
                        let eb = entry.borrow();
                        if eb.len() > 0 {
                            let k = eb.get(0);
                            let v = if eb.len() > 1 { eb.get(1) } else { Value::undefined() };
                            container_insert(&m, k, v, vm);
                        }
                    }
                }
            }
        }
        m
    });
    Value::native_ctor(ctor, proto)
}

/// The `Set` constructor — same shape as [`make_map_ctor`] with container 2.
pub(crate) fn make_set_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    for name in ["add", "has", "delete", "clear", "keys", "values", "entries", "forEach"] {
        if let Some(od) = proto.as_object() {
            od.borrow_mut().set(name, container_method_native(name));
        }
    }
    if let Some(od) = proto.as_object() {
        od.borrow_mut().set("\0sym_1", container_method_native("values"));
    }
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |args: &[Value], vm: &mut dyn VmHost| {
        let s = Value::map(ctor_proto.clone(), 2);
        if let Some(seed) = args.first() {
            if let Some(arr) = seed.as_array() {
                for item in arr.borrow().to_values() {
                    container_insert(&s, item.clone(), item, vm);
                }
            }
        }
        s
    });
    Value::native_ctor(ctor, proto)
}


/// Message-passing concurrency primitive: `channel.create()` returns a
/// bidirectional FIFO. `send` resolves the oldest pending `recv()` promise (or
/// buffers), `recv` takes the oldest message (or parks on a promise the event
/// loop resolves when a `send` lands). `await ch.recv()` therefore suspends the
/// async function on an empty channel exactly like a message queue in any
/// actor runtime.
/// Named channels, shared across VMs and threads: `channel.create("jobs")`
/// registers the channel here and `channel.get("jobs")` (from a spawn
/// worker's isolated VM, or the reverse) returns the same state — the only
/// way two VMs can share a channel, since Values (arena pointers) cannot be
/// serialized. Named channels therefore carry their messages as bytes (see
/// `ChannelItem`). Entries live for the process, like a global registry.
static NAMED_CHANNELS: std::sync::OnceLock<
    Mutex<HashMap<String, Arc<Mutex<ChannelState>>>>,
> = std::sync::OnceLock::new();

pub(crate) fn named_channels() -> &'static Mutex<HashMap<String, Arc<Mutex<ChannelState>>>> {
    NAMED_CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn make_channel_module() -> Value {
    let create = Value::native(Arc::new(|args, _vm| {
        let name = args.first().and_then(|v| v.as_str()).map(|s| s.to_string());
        let ch = Value::channel(Arc::new(Mutex::new(ChannelState {
            queue: VecDeque::new(),
            waiters: VecDeque::new(),
            // Named channels cross VMs, so their messages must travel as
            // bytes; anonymous channels stay per-VM and pass raw values.
            named: name.is_some(),
        })));
        if let Some(n) = name {
            let arc = ch.as_channel().expect("channel").clone();
            named_channels()
                .lock()
                .unwrap_or_else(|g| g.into_inner())
                .insert(n, arc);
        }
        ch
    }));
    let get = Value::native(Arc::new(|args, vm| {
        match args.first().and_then(|v| v.as_str()) {
            Some(name) => named_channels()
                .lock()
                .unwrap_or_else(|g| g.into_inner())
                .get(name)
                .map(|a| Value::channel(a.clone()))
                .unwrap_or_else(|| {
                    vm.throw_exception(Value::string(format!(
                        "Error: channel '{}' not found — create it with channel.create('{}')",
                        name, name
                    )));
                    Value::undefined()
                }),
            None => Value::undefined(),
        }
    }));
    let mut m = HashMap::new();
    m.insert("create".to_string(), create);
    m.insert("get".to_string(), get);
    Value::object(m)
}

