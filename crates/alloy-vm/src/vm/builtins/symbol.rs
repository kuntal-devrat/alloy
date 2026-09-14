use alloy_core::value::{
    symbol_description, symbol_for, symbol_key_for, symbol_new, NativeFn, Value,
    SYMBOL_ASYNC_ITERATOR, SYMBOL_HAS_INSTANCE, SYMBOL_IS_CONCAT_SPREADABLE, SYMBOL_ITERATOR,
    SYMBOL_SPECIES, SYMBOL_TO_PRIMITIVE, SYMBOL_TO_STRING_TAG,
};
use std::sync::Arc;

pub(crate) fn make_symbol_ctor() -> Value {
    // Prototype object for Symbol instances / values
    let to_string = Value::native(Arc::new(|_args, vm| {
        let th = vm.this_value();
        if let Some(id) = th.as_symbol() {
            match symbol_description(id) {
                Some(desc) => Value::string(format!("Symbol({})", desc)),
                None => Value::string("Symbol()".to_string()),
            }
        } else {
            vm.throw_exception(Value::string(
                "TypeError: Symbol.prototype.toString requires that 'this' be a Symbol".to_string(),
            ));
            Value::undefined()
        }
    }));

    let value_of = Value::native(Arc::new(|_args, vm| {
        let th = vm.this_value();
        if th.is_symbol() {
            th
        } else {
            vm.throw_exception(Value::string(
                "TypeError: Symbol.prototype.valueOf requires that 'this' be a Symbol".to_string(),
            ));
            Value::undefined()
        }
    }));

    let mut proto_props = hashbrown::HashMap::new();
    proto_props.insert("toString".to_string(), to_string);
    proto_props.insert("valueOf".to_string(), value_of);
    let proto = Value::object(proto_props);

    // Static Symbol.for(key)
    let for_fn = Value::native(Arc::new(|args, _vm| {
        let key = args.first().and_then(|v| v.as_str()).unwrap_or("undefined");
        symbol_for(key)
    }));

    // Static Symbol.keyFor(sym)
    let key_for_fn = Value::native(Arc::new(|args, vm| {
        let sym = args.first().cloned().unwrap_or(Value::undefined());
        if !sym.is_symbol() {
            vm.throw_exception(Value::string(
                "TypeError: Symbol.keyFor requires that argument be a symbol".to_string(),
            ));
            return Value::undefined();
        }
        match symbol_key_for(&sym) {
            Some(k) => Value::string(k),
            None => Value::undefined(),
        }
    }));

    let ctor_fn: NativeFn = Arc::new(|args, vm| {
        // Calling with `new` is disallowed in JS
        if vm.this_value().is_object() {
            vm.throw_exception(Value::string(
                "TypeError: Symbol is not a constructor".to_string(),
            ));
            return Value::undefined();
        }
        let desc = args
            .first()
            .and_then(|v| if v.is_undefined() { None } else { v.as_str() });
        symbol_new(desc)
    });

    let statics = vec![
        ("for".to_string(), for_fn),
        ("keyFor".to_string(), key_for_fn),
        ("iterator".to_string(), Value::symbol(SYMBOL_ITERATOR)),
        (
            "toStringTag".to_string(),
            Value::symbol(SYMBOL_TO_STRING_TAG),
        ),
        (
            "hasInstance".to_string(),
            Value::symbol(SYMBOL_HAS_INSTANCE),
        ),
        (
            "toPrimitive".to_string(),
            Value::symbol(SYMBOL_TO_PRIMITIVE),
        ),
        (
            "isConcatSpreadable".to_string(),
            Value::symbol(SYMBOL_IS_CONCAT_SPREADABLE),
        ),
        ("species".to_string(), Value::symbol(SYMBOL_SPECIES)),
        (
            "asyncIterator".to_string(),
            Value::symbol(SYMBOL_ASYNC_ITERATOR),
        ),
    ];

    Value::native_with_props(ctor_fn, proto, statics)
}
