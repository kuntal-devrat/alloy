use std::sync::Arc;
use alloy_core::value::Value;

pub(crate) fn make_proxy_ctor() -> Value {
    let ctor = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let handler = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_function() && !target.is_native() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Cannot create proxy with a non-object as target or handler".to_string()));
            return Value::undefined();
        }
        if !handler.is_object() && !handler.is_function() && !handler.is_native() {
            vm.throw_exception(Value::string("TypeError: Cannot create proxy with a non-object as target or handler".to_string()));
            return Value::undefined();
        }
        Value::proxy(target, handler)
    }));

    let revocable = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let handler = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_function() && !target.is_native() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Cannot create proxy with a non-object as target or handler".to_string()));
            return Value::undefined();
        }
        if !handler.is_object() && !handler.is_function() && !handler.is_native() {
            vm.throw_exception(Value::string("TypeError: Cannot create proxy with a non-object as target or handler".to_string()));
            return Value::undefined();
        }
        let proxy_val = Value::proxy(target, handler);
        let proxy_arc = proxy_val.as_proxy().unwrap().clone();
        let revoke_fn = Value::native(Arc::new(move |_args, _vm| {
            let mut guard = proxy_arc.lock().unwrap_or_else(|g| g.into_inner());
            guard.revoked = true;
            Value::undefined()
        }));
        let mut obj = hashbrown::HashMap::new();
        obj.insert("proxy".to_string(), proxy_val);
        obj.insert("revoke".to_string(), revoke_fn);
        Value::object(obj)
    }));

    let statics = vec![
        ("revocable".to_string(), revocable),
    ];

    Value::native_with_props(
        match ctor.as_native() {
            Some(f) => f.clone(),
            None => unreachable!(),
        },
        Value::undefined(),
        statics,
    )
}

pub(crate) fn make_reflect_module() -> Value {
    let get = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let prop = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Reflect.get called on non-object".to_string()));
            return Value::undefined();
        }
        if let Some(s) = prop.as_str() {
            if let Some(od) = target.as_object() {
                return od.borrow().get(s).cloned().unwrap_or(Value::undefined());
            }
        }
        if let Some(id) = prop.as_symbol() {
            let key = format!("\0sym_{}", id);
            if let Some(od) = target.as_object() {
                return od.borrow().get(&key).cloned().unwrap_or(Value::undefined());
            }
        }
        Value::undefined()
    }));

    let set = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let prop = args.get(1).cloned().unwrap_or(Value::undefined());
        let val = args.get(2).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Reflect.set called on non-object".to_string()));
            return Value::bool(false);
        }
        if let Some(od) = target.as_object() {
            if let Some(s) = prop.as_str() {
                od.borrow_mut().set(s, val);
                return Value::bool(true);
            }
            if let Some(id) = prop.as_symbol() {
                let key = format!("\0sym_{}", id);
                od.borrow_mut().set(&key, val);
                return Value::bool(true);
            }
        }
        Value::bool(false)
    }));

    let has = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let prop = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Reflect.has called on non-object".to_string()));
            return Value::bool(false);
        }
        if let Some(od) = target.as_object() {
            if let Some(s) = prop.as_str() {
                return Value::bool(od.borrow().get(s).is_some());
            }
            if let Some(id) = prop.as_symbol() {
                let key = format!("\0sym_{}", id);
                return Value::bool(od.borrow().get(&key).is_some());
            }
        }
        Value::bool(false)
    }));

    let delete_property = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let prop = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Reflect.deleteProperty called on non-object".to_string()));
            return Value::bool(false);
        }
        if let Some(od) = target.as_object() {
            if let Some(s) = prop.as_str() {
                return Value::bool(od.borrow_mut().delete(s));
            }
            if let Some(id) = prop.as_symbol() {
                let key = format!("\0sym_{}", id);
                return Value::bool(od.borrow_mut().delete(&key));
            }
        }
        Value::bool(true)
    }));

    let own_keys = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() && !target.is_proxy() {
            vm.throw_exception(Value::string("TypeError: Reflect.ownKeys called on non-object".to_string()));
            return Value::undefined();
        }
        if let Some(od) = target.as_object() {
            let mut keys = Vec::new();
            for k in od.borrow().keys_live() {
                if let Some(sym_str) = k.strip_prefix("\0sym_") {
                    if let Ok(id) = sym_str.parse::<u64>() {
                        keys.push(Value::symbol(id));
                    }
                } else if !k.starts_with('\0') {
                    keys.push(Value::string(k.to_string()));
                }
            }
            return Value::array(keys);
        }
        Value::array_empty()
    }));

    let apply = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
        let arg_list = args.get(2).cloned().unwrap_or(Value::undefined());
        if !target.is_function() && !target.is_native() {
            vm.throw_exception(Value::string("TypeError: Reflect.apply called on non-function".to_string()));
            return Value::undefined();
        }
        let rest: Vec<Value> = if let Some(arr) = arg_list.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        // vm.call_value_with_this isn't directly on VmHost, but we can do a fallback or check VmHost
        // If native target:
        if let Some(nf) = target.as_native() {
            return nf(&rest, vm);
        }
        Value::undefined()
    }));

    let get_proto = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() {
            vm.throw_exception(Value::string("TypeError: Reflect.getPrototypeOf called on non-object".to_string()));
            return Value::undefined();
        }
        if let Some(od) = target.as_object() {
            return od.borrow().proto.clone();
        }
        Value::null()
    }));

    let set_proto = Value::native(Arc::new(|args, vm| {
        let target = args.first().cloned().unwrap_or(Value::undefined());
        let proto = args.get(1).cloned().unwrap_or(Value::undefined());
        if !target.is_object() && !target.is_array() {
            vm.throw_exception(Value::string("TypeError: Reflect.setPrototypeOf called on non-object".to_string()));
            return Value::bool(false);
        }
        if let Some(od) = target.as_object() {
            od.borrow_mut().proto = proto;
            return Value::bool(true);
        }
        Value::bool(false)
    }));

    let mut m = hashbrown::HashMap::new();
    m.insert("get".to_string(), get);
    m.insert("set".to_string(), set);
    m.insert("has".to_string(), has);
    m.insert("deleteProperty".to_string(), delete_property);
    m.insert("ownKeys".to_string(), own_keys);
    m.insert("apply".to_string(), apply);
    m.insert("getPrototypeOf".to_string(), get_proto);
    m.insert("setPrototypeOf".to_string(), set_proto);
    Value::object(m)
}
