use super::containers::container_pairs;
use crate::vm::alu::strict_equal;
use crate::vm::core::unwrap_cell;
use alloy_core::value::{to_string_js, ArrayData, ObjectData, Value, VmHost};
use hashbrown::HashMap;
use std::sync::{Arc, Mutex};

/// Stable bottom-up merge sort (V8's sort is stable; ES2019 requires it).
/// `cmp` must return `Less`/`Equal`/`Greater`; equal elements keep their
/// input order.
pub(crate) fn stable_merge_sort<T: Clone>(
    v: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> std::cmp::Ordering,
) {
    let n = v.len();
    if n <= 1 {
        return;
    }
    let mut aux: Vec<T> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        let mut i = 0;
        while i < n {
            let lo = i;
            let mid = (i + width).min(n);
            let hi = (i + 2 * width).min(n);
            aux.clear();
            let (mut a, mut b) = (lo, mid);
            while a < mid && b < hi {
                if cmp(&v[a], &v[b]) != std::cmp::Ordering::Greater {
                    aux.push(v[a].clone());
                    a += 1;
                } else {
                    aux.push(v[b].clone());
                    b += 1;
                }
            }
            while a < mid {
                aux.push(v[a].clone());
                a += 1;
            }
            while b < hi {
                aux.push(v[b].clone());
                b += 1;
            }
            for (k, x) in aux.iter().enumerate() {
                v[lo + k] = x.clone();
            }
            i = hi;
        }
        width *= 2;
    }
}

pub(crate) fn iterable_display(v: &Value) -> String {
    if v.is_undefined() {
        return "undefined".to_string();
    }
    if v.is_null() {
        return "null".to_string();
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if v.is_number() || v.is_int() {
        return format!("{}", v);
    }
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if v.is_object() {
        let empty = match v.as_object() {
            Some(od) => od.borrow().shape.is_empty(),
            None => true,
        };
        return if empty {
            "{}".to_string()
        } else {
            "{...}".to_string()
        };
    }
    format!("{}", v)
}

/// Own enumerable properties of `src` in insertion order, for object spread
/// `{...src}`: plain objects walk the shape (tombstones skipped, cells
/// unwrapped); arrays yield their indices `0..len`; strings yield their
/// character indices (all match JS). Everything else — numbers, booleans,
/// functions, null, undefined — has no own enumerable props, so `None` makes
/// the spread a no-op. Map/Set return `None` too: in JS they have no own
/// enumerable props.
pub(crate) fn object_spread_pairs(src: &Value) -> Option<Vec<(String, Value)>> {
    if let Some(od) = src.as_object() {
        if od.borrow().container != 0 {
            return None;
        }
        let od = od.borrow();
        let mut out = Vec::with_capacity(od.shape.len());
        for (k, off) in od.shape.keys_by_offset() {
            let off = off as usize;
            if !od.deleted[off] {
                out.push((k.clone(), unwrap_cell(od.values[off].clone())));
            }
        }
        return Some(out);
    }
    if let Some(arr) = src.as_array() {
        let arr = arr.borrow();
        let mut out = Vec::with_capacity(arr.len());
        for i in 0..arr.len() {
            out.push((i.to_string(), arr.get(i)));
        }
        return Some(out);
    }
    if let Some(s) = src.as_str() {
        let mut out = Vec::with_capacity(s.chars().count());
        for (i, ch) in s.chars().enumerate() {
            out.push((i.to_string(), Value::string(ch.to_string())));
        }
        return Some(out);
    }
    None
}

/// `key in obj` existence probe: `Some(true/false)` for object/function/
/// array targets (own + prototype chain), `None` for everything else
/// (primitives and Map/Set), which the caller reports as a TypeError.
pub(crate) fn in_operator_probe(obj: &Value, key: &str) -> Option<bool> {
    if let Some(od) = obj.as_object() {
        if od.borrow().container != 0 {
            return None;
        }
        let mut cur = obj.clone();
        for _ in 0..1024 {
            let Some(c) = cur.as_object() else { break };
            let (hit, next) = {
                let b = c.borrow();
                (
                    b.shape.get(key).is_some_and(|off| !b.deleted[off as usize]),
                    b.proto.clone(),
                )
            };
            if hit {
                return Some(true);
            }
            cur = next;
        }
        return Some(false);
    }
    if let Some(arr) = obj.as_array() {
        // Canonical array-index keys (`0 in a`), then the synthesized
        // prototype methods (`"map" in []`). Non-index keys on an array
        // resolve through the same synthesized surface.
        if let Ok(n) = key.parse::<usize>() {
            if n < arr.borrow().len() {
                return Some(true);
            }
        }
        // No Array.prototype object exists, so method names probe the
        // synthesized surface conservatively.
        return Some(!array_prop(obj, key).is_undefined());
    }
    if let Some(f) = obj.as_function() {
        return Some(
            f.props
                .borrow()
                .as_ref()
                .is_some_and(|p| p.borrow().contains_key(key)),
        );
    }
    None
}

pub(crate) fn array_prop(obj: &Value, name: &str) -> Value {
    let arr = obj.clone();
    match name {
        "length" => Value::int(
            arr.as_array()
                .map(|ad| ad.borrow().len() as i64)
                .unwrap_or(0),
        ),
        "push" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut ad = ad.borrow_mut();
                for a in args {
                    ad.push(a.clone());
                }
                Value::int(ad.len() as i64)
            } else {
                Value::undefined()
            }
        })),
        "join" => Value::native(Arc::new(move |args, _vm| {
            let sep = match args.first() {
                Some(v) if v.is_undefined() || v.is_null() => ",".to_string(),
                Some(v) => to_string_js(v),
                None => ",".to_string(),
            };
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                // Packed-int fast path: format i64s directly, no Value boxing.
                match &*ad {
                    alloy_core::value::ArrayData::Ints(vs) => {
                        let mut out = String::with_capacity(vs.len() * 3);
                        for (i, n) in vs.iter().enumerate() {
                            if i > 0 {
                                out.push_str(&sep);
                            }
                            out.push_str(&n.to_string());
                        }
                        Value::string(out)
                    }
                    alloy_core::value::ArrayData::Values(vs) => {
                        let mut parts = Vec::with_capacity(vs.len());
                        for e in vs.iter() {
                            if e.is_null() || e.is_undefined() {
                                parts.push(String::new());
                            } else {
                                parts.push(to_string_js(e));
                            }
                        }
                        Value::string(parts.join(&sep))
                    }
                }
            } else {
                Value::undefined()
            }
        })),
        "pop" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                ad.borrow_mut().pop()
            } else {
                Value::undefined()
            }
        })),
        "shift" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                ad.borrow_mut().shift()
            } else {
                Value::undefined()
            }
        })),
        "unshift" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let items: Vec<Value> = args.to_vec();
                let len = ad.borrow_mut().unshift_front(&items);
                Value::int(len as i64)
            } else {
                Value::undefined()
            }
        })),
        "slice" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let n = ad.len() as i64;
                let to_i64 = |v: &Value| -> i64 {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                };
                let start = match args.first() {
                    Some(v) if v.is_undefined() => 0i64,
                    Some(v) => {
                        let mut s = to_i64(v);
                        if s < 0 {
                            s = (n + s).max(0);
                        }
                        s.min(n)
                    }
                    None => 0,
                };
                let end = match args.get(1) {
                    Some(v) if v.is_undefined() => n,
                    Some(v) => {
                        let mut e = to_i64(v);
                        if e < 0 {
                            e = (n + e).max(0);
                        }
                        e.min(n)
                    }
                    None => n,
                };
                let mut out: Vec<Value> = Vec::new();
                let mut i = start;
                while i < end {
                    out.push(ad.get(i as usize));
                    i += 1;
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "concat" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut out = ad.borrow().to_values();
                for a in args {
                    if let Some(other) = a.as_array() {
                        out.extend(other.borrow().to_values());
                    } else {
                        out.push(a.clone());
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "indexOf" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let needle = args.first().cloned().unwrap_or(Value::undefined());
                let from = args
                    .get(1)
                    .map(|v| {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else {
                            x.trunc() as i64
                        }
                    })
                    .unwrap_or(0);
                let n = ad.len() as i64;
                let mut i = if from < 0 {
                    (n + from).max(0)
                } else {
                    from.min(n)
                };
                while i < n {
                    if strict_equal(&ad.get(i as usize), &needle) {
                        return Value::int(i);
                    }
                    i += 1;
                }
                Value::int(-1)
            } else {
                Value::undefined()
            }
        })),
        "includes" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let needle = args.first().cloned().unwrap_or(Value::undefined());
                let from = args
                    .get(1)
                    .map(|v| {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else {
                            x.trunc() as i64
                        }
                    })
                    .unwrap_or(0);
                let n = ad.len() as i64;
                let mut i = if from < 0 {
                    (n + from).max(0)
                } else {
                    from.min(n)
                };
                let needle_nan = needle.is_number() && needle.to_number().is_nan();
                while i < n {
                    let e = ad.get(i as usize);
                    if needle_nan {
                        if e.is_number() && e.to_number().is_nan() {
                            return Value::bool(true);
                        }
                    } else if strict_equal(&e, &needle) {
                        return Value::bool(true);
                    }
                    i += 1;
                }
                Value::bool(false)
            } else {
                Value::undefined()
            }
        })),
        "map" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out: Vec<Value> = Vec::with_capacity(ad.len());
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    out.push(vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "forEach" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                }
            }
            Value::undefined()
        })),
        "find" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return e.clone();
                    }
                }
            }
            Value::undefined()
        })),
        "findIndex" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::int(i as i64);
                    }
                }
            }
            Value::int(-1)
        })),
        "filter" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut out: Vec<Value> = Vec::new();
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        out.push(e.clone());
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "some" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::bool(true);
                    }
                }
            }
            Value::bool(false)
        })),
        "every" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if !r.is_truthy() {
                        return Value::bool(false);
                    }
                }
            }
            Value::bool(true)
        })),
        "reverse" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                match &mut *guard {
                    ArrayData::Ints(vs) => vs.reverse(),
                    ArrayData::Values(vs) => vs.reverse(),
                }
            }
            // JS returns the same (mutated) array.
            arr.clone()
        })),
        "sort" => Value::native(Arc::new(move |args, vm| {
            let cmp = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let mut vals = guard.to_values();
                // ES: undefined always sorts to the end, even with a
                // comparator.
                let mut undefs = 0usize;
                vals.retain(|v| {
                    if v.is_undefined() {
                        undefs += 1;
                        false
                    } else {
                        true
                    }
                });
                if cmp.is_undefined() {
                    // Default order: ToString each element once, compare
                    // lexicographically by code unit — `[10, 9]` sorts as
                    // `[10, 9]` ("10" < "9"), matching Node/V8.
                    let mut pairs: Vec<(String, Value)> =
                        vals.into_iter().map(|v| (to_string_js(&v), v)).collect();
                    stable_merge_sort(&mut pairs, |a, b| a.0.cmp(&b.0));
                    vals = pairs.into_iter().map(|(_, v)| v).collect();
                } else {
                    let cmp2 = cmp.clone();
                    stable_merge_sort(&mut vals, |a, b| {
                        let r = vm.call_value(&cmp2, &[a.clone(), b.clone()]);
                        let n = r.to_number();
                        if n < 0.0 {
                            std::cmp::Ordering::Less
                        } else if n > 0.0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    });
                }
                for _ in 0..undefs {
                    vals.push(Value::undefined());
                }
                // Write back into the same box; int-only results stay packed.
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
            }
            arr.clone()
        })),
        // `reduce(cb, initial?)`: left-to-right accumulator fold. No initial
        // value uses the first element as the accumulator and starts at 1.
        "reduce" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut start = 0usize;
                let mut acc = args.get(1).cloned().unwrap_or(Value::undefined());
                if args.get(1).is_none() {
                    if vals.is_empty() {
                        vm.throw_exception(Value::string(
                            "TypeError: Reduce of empty array with no initial value".to_string(),
                        ));
                        return Value::undefined();
                    }
                    acc = vals[0].clone();
                    start = 1;
                }
                for i in start..vals.len() {
                    acc = vm.call_value(
                        &cb,
                        &[acc, vals[i].clone(), Value::int(i as i64), arr.clone()],
                    );
                }
                acc
            } else {
                Value::undefined()
            }
        })),
        // `reduceRight(cb, initial?)`: right-to-left fold (indexes still pass
        // as the element's own index).
        "reduceRight" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let n = vals.len();
                let mut acc = args.get(1).cloned().unwrap_or(Value::undefined());
                if args.get(1).is_none() {
                    if n == 0 {
                        vm.throw_exception(Value::string(
                            "TypeError: Reduce of empty array with no initial value".to_string(),
                        ));
                        return Value::undefined();
                    }
                    acc = vals[n - 1].clone();
                }
                let last = if args.get(1).is_none() { n - 1 } else { n };
                for i in (0..last).rev() {
                    acc = vm.call_value(
                        &cb,
                        &[acc, vals[i].clone(), Value::int(i as i64), arr.clone()],
                    );
                }
                acc
            } else {
                Value::undefined()
            }
        })),
        // `splice(start, deleteCount, ...items)`: remove `deleteCount`
        // elements from `start` and insert `items` in their place; returns the
        // removed elements as a new array.
        "splice" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let mut vals = guard.to_values();
                let n = vals.len() as i64;
                let to_i64 = |v: &Value| {
                    let x = v.to_number();
                    if x.is_nan() {
                        0
                    } else {
                        x.trunc() as i64
                    }
                };
                let start = match args.first() {
                    Some(v) if v.is_undefined() => 0i64,
                    Some(v) => {
                        let mut s = to_i64(v);
                        if s < 0 {
                            s = (n + s).max(0);
                        }
                        s.min(n)
                    }
                    None => 0,
                };
                let del = match args.get(1) {
                    Some(v) if v.is_undefined() => (n - start).max(0),
                    Some(v) => to_i64(v).clamp(0, (n - start).max(0)),
                    None => (n - start).max(0),
                };
                let items: Vec<Value> = args.iter().skip(2).cloned().collect();
                let removed: Vec<Value> =
                    vals.drain(start as usize..(start + del) as usize).collect();
                for (j, it) in items.iter().enumerate() {
                    vals.insert((start + j as i64) as usize, it.clone());
                }
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
                Value::array(removed)
            } else {
                Value::undefined()
            }
        })),
        // `flat(depth?)`: recursively flatten nested arrays up to `depth`
        // (default 1); every level deeper than depth stays as-is.
        "flat" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let depth = match args.first() {
                    Some(v) if v.is_undefined() => 1usize,
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else if x.is_infinite() {
                            if x > 0.0 {
                                usize::MAX
                            } else {
                                0
                            }
                        } else {
                            x.trunc().max(0.0) as usize
                        }
                    }
                    None => 1,
                };
                let mut out: Vec<Value> = Vec::new();
                fn push_flat(src: &Value, depth: usize, out: &mut Vec<Value>) {
                    if depth > 0 {
                        if let Some(inner) = src.as_array() {
                            let inner = inner.borrow();
                            for e in inner.to_values() {
                                push_flat(&e, depth - 1, out);
                            }
                            return;
                        }
                    }
                    out.push(src.clone());
                }
                for e in ad.to_values() {
                    push_flat(&e, depth, &mut out);
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `flatMap(cb)`: map then flat(1).
        "flatMap" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                let mut out: Vec<Value> = Vec::new();
                for (i, e) in vals.iter().enumerate() {
                    let r = vm.call_value(&cb, &[e.clone(), Value::int(i as i64), arr.clone()]);
                    if let Some(inner) = r.as_array() {
                        out.extend(inner.borrow().to_values());
                    } else {
                        out.push(r);
                    }
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `at(i)`: index, negative counts from the end; out of bounds is
        // undefined (unlike `[]` which returns undefined too but never
        // negative-clamps).
        "at" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let n = ad.len() as i64;
                let i = match args.first() {
                    Some(v) => {
                        let x = v.to_number();
                        if x.is_nan() {
                            0
                        } else {
                            x.trunc() as i64
                        }
                    }
                    None => 0,
                };
                let idx = if i < 0 { n + i } else { i };
                if idx < 0 || idx >= n {
                    Value::undefined()
                } else {
                    ad.get(idx as usize)
                }
            } else {
                Value::undefined()
            }
        })),
        // `fill(value, start?, end?)`: write `value` into [start, end)
        // (negative counts from the end), returns the same array.
        "fill" => Value::native(Arc::new(move |args, _vm| {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let n = guard.len() as i64;
                let arg = |i: usize, dflt: i64| -> i64 {
                    match args.get(i) {
                        Some(v) if v.is_undefined() => dflt,
                        Some(v) => {
                            let x = v.to_number();
                            if x.is_nan() {
                                0
                            } else {
                                x.trunc() as i64
                            }
                        }
                        None => dflt,
                    }
                };
                let mut a = arg(1, 0);
                let mut b = arg(2, n);
                if a < 0 {
                    a = (n + a).max(0);
                }
                if b < 0 {
                    b = (n + b).max(0);
                }
                a = a.min(n);
                b = b.min(n);
                let mut vals = guard.to_values();
                for i in a..b {
                    vals[i as usize] = value.clone();
                }
                let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                    ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                } else {
                    ArrayData::Values(vals)
                };
                *guard = new_data;
            }
            arr.clone()
        })),
        // `copyWithin(target, start, end?)`: copy the slice [start, end) to
        // `target` (negative counts from the end), returning the same array.
        "copyWithin" => Value::native(Arc::new(move |args, _vm| {
            if let Some(ad) = arr.as_array() {
                let mut guard = ad.borrow_mut();
                let n = guard.len() as i64;
                let arg = |i: usize, dflt: i64| -> i64 {
                    match args.get(i) {
                        Some(v) if v.is_undefined() => dflt,
                        Some(v) => {
                            let x = v.to_number();
                            if x.is_nan() {
                                0
                            } else {
                                x.trunc() as i64
                            }
                        }
                        None => dflt,
                    }
                };
                let mut t = arg(0, 0);
                let mut a = arg(1, 0);
                let mut b = arg(2, n);
                if t < 0 {
                    t = (n + t).max(0);
                }
                if a < 0 {
                    a = (n + a).max(0);
                }
                if b < 0 {
                    b = (n + b).max(0);
                }
                t = t.min(n);
                a = a.min(n);
                b = b.min(n);
                if a < b && t < n {
                    let mut vals = guard.to_values();
                    let count = (b - a).min(n - t);
                    // Memmove semantics: a forward copy when target < start
                    // would clobber the source, so copy in the safe direction.
                    if t <= a {
                        for k in 0..count {
                            vals[(t + k) as usize] = vals[(a + k) as usize].clone();
                        }
                    } else {
                        for k in (0..count).rev() {
                            vals[(t + k) as usize] = vals[(a + k) as usize].clone();
                        }
                    }
                    let new_data = if vals.iter().all(|v| v.as_int().is_some()) {
                        ArrayData::Ints(vals.iter().map(|v| v.as_int().unwrap()).collect())
                    } else {
                        ArrayData::Values(vals)
                    };
                    *guard = new_data;
                }
            }
            arr.clone()
        })),
        // `keys()` / `values()` / `entries()` — array snapshots (the engine's
        // container convention; see Map/Set) of indices, elements, and
        // [index, element] pairs.
        "keys" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out = Vec::new();
                for i in 0..ad.len() {
                    out.push(Value::int(i as i64));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        "values" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                Value::array(ad.borrow().to_values())
            } else {
                Value::undefined()
            }
        })),
        "entries" => Value::native(Arc::new(move |_args, _vm| {
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let mut out = Vec::new();
                for (i, e) in ad.to_values().iter().enumerate() {
                    out.push(Value::array(vec![Value::int(i as i64), e.clone()]));
                }
                Value::array(out)
            } else {
                Value::undefined()
            }
        })),
        // `findLast` / `findLastIndex`: like find/findIndex, scanning from the
        // end.
        "findLast" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for i in (0..vals.len()).rev() {
                    let r =
                        vm.call_value(&cb, &[vals[i].clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return vals[i].clone();
                    }
                }
            }
            Value::undefined()
        })),
        "findLastIndex" => Value::native(Arc::new(move |args, vm| {
            let cb = args.first().cloned().unwrap_or(Value::undefined());
            if let Some(ad) = arr.as_array() {
                let ad = ad.borrow();
                let vals = ad.to_values();
                drop(ad);
                for i in (0..vals.len()).rev() {
                    let r =
                        vm.call_value(&cb, &[vals[i].clone(), Value::int(i as i64), arr.clone()]);
                    if r.is_truthy() {
                        return Value::int(i as i64);
                    }
                }
            }
            Value::int(-1)
        })),
        "\0sym_1" => Value::native(Arc::new(move |_args, _vm| {
            let ad = arr
                .as_array()
                .map(|a| a.borrow().to_values())
                .unwrap_or_default();
            make_array_iterator(ad)
        })),
        _ => Value::undefined(),
    }
}

pub(crate) fn make_array_iterator(items: Vec<Value>) -> Value {
    let idx = Arc::new(Mutex::new(0usize));
    let items = Arc::new(items);
    let next_fn = {
        let idx = idx.clone();
        let items = items.clone();
        Value::native(Arc::new(move |_args, _vm| {
            let mut i = idx.lock().unwrap();
            let mut m = HashMap::new();
            if *i < items.len() {
                let val = items[*i].clone();
                *i += 1;
                m.insert("value".to_string(), val);
                m.insert("done".to_string(), Value::bool(false));
            } else {
                m.insert("value".to_string(), Value::undefined());
                m.insert("done".to_string(), Value::bool(true));
            }
            Value::object(m)
        }))
    };
    let mut props = HashMap::new();
    props.insert("next".to_string(), next_fn);
    let iter_obj = Value::object(props);
    if let Some(od) = iter_obj.as_object() {
        let self_fn = Value::native(Arc::new(move |_args, vm| vm.this_value()));
        od.borrow_mut().set("\0sym_1", self_fn);
    }
    iter_obj
}

/// Is `s` a canonical ECMAScript array index ("0".."4294967294", no leading
/// zeros)? Such keys enumerate FIRST — ascending — in Object.keys/values/
/// entries and JSON.stringify; all other string keys follow in insertion
/// order.
pub(crate) fn is_array_index_key(s: &str) -> bool {
    if s.is_empty() || s.len() > 10 {
        return false;
    }
    if s == "0" {
        return true;
    }
    if s.starts_with('0') {
        return false;
    }
    s.parse::<u64>().is_ok_and(|n| n < 4294967295)
}

/// A plain object's own (non-deleted) property entries in JS enumeration
/// order: integer-index keys ascending, then the remaining string keys in
/// insertion order. Returns (name, offset) pairs.
pub(crate) fn object_keys_js_order(od: &ObjectData) -> Vec<(String, usize)> {
    let mut ints: Vec<(String, usize)> = Vec::new();
    let mut rest: Vec<(String, usize)> = Vec::new();
    for (k, off) in od.shape.keys_by_offset() {
        let off = off as usize;
        if od.deleted[off] {
            continue;
        }
        if is_array_index_key(k) {
            ints.push((k.clone(), off));
        } else {
            rest.push((k.clone(), off));
        }
    }
    ints.sort_by(|a, b| {
        a.0.parse::<u64>()
            .unwrap_or(0)
            .cmp(&b.0.parse::<u64>().unwrap_or(0))
    });
    ints.extend(rest);
    ints
}

/// ToObject + own-enumerable enumeration for Object.keys/values/entries:
/// null/undefined throw a TypeError (like Node), numbers/booleans have no
/// own keys, strings enumerate their character indices (with the character
/// as the value), objects enumerate their shape entries. `None` means a
/// TypeError was thrown.
pub(crate) fn object_own_entries(arg: &Value, vm: &mut dyn VmHost) -> Option<Vec<(String, Value)>> {
    if arg.is_null() || arg.is_undefined() {
        // Node's exact message for both null and undefined.
        vm.throw_exception(Value::string(
            "TypeError: Cannot convert undefined or null to object".to_string(),
        ));
        return None;
    }
    if let Some(od) = arg.as_object() {
        let od = od.borrow();
        return Some(
            object_keys_js_order(&od)
                .into_iter()
                .map(|(k, off)| (k, unwrap_cell(od.values[off].clone())))
                .collect(),
        );
    }
    if let Some(s) = arg.as_str() {
        return Some(
            s.chars()
                .enumerate()
                .map(|(i, c)| (i.to_string(), Value::string(c.to_string())))
                .collect(),
        );
    }
    Some(Vec::new())
}

/// `Object` global: keys/values/entries with Node's enumeration order and
/// coercion rules (the Map/Set iteration surface already returns array
/// snapshots; this extends the same shape to plain objects).
pub(crate) fn make_object_module() -> Value {
    let keys = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(entries) => {
                Value::array(entries.into_iter().map(|(k, _)| Value::string(k)).collect())
            }
            None => Value::undefined(),
        }
    }));
    let values = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(entries) => Value::array(entries.into_iter().map(|(_, v)| v).collect()),
            None => Value::undefined(),
        }
    }));
    let entries = Value::native(Arc::new(|args, vm| {
        let arg = args.first().cloned().unwrap_or(Value::undefined());
        match object_own_entries(&arg, vm) {
            Some(list) => Value::array(
                list.into_iter()
                    .map(|(k, v)| Value::array(vec![Value::string(k), v]))
                    .collect(),
            ),
            None => Value::undefined(),
        }
    }));
    let has_own = Value::native(Arc::new(|args, vm| {
        let obj = args.first().cloned().unwrap_or(Value::undefined());
        let prop = args.get(1).cloned().unwrap_or(Value::undefined());
        if obj.is_null() || obj.is_undefined() {
            vm.throw_exception(Value::string(
                "TypeError: Cannot convert undefined or null to object".to_string(),
            ));
            return Value::undefined();
        }
        if let Some(od) = obj.as_object() {
            let od = od.borrow();
            if let Some(id) = prop.as_symbol() {
                return Value::bool(od.get(&format!("\0sym_{}", id)).is_some());
            }
            let key = match prop.as_str() {
                Some(s) => s.to_string(),
                None => format!("{}", prop),
            };
            return Value::bool(od.get(&key).is_some());
        }
        if let Some(arr) = obj.as_array() {
            if let Some(s) = prop.as_str() {
                if s == "length" {
                    return Value::bool(true);
                }
            }
            let i = prop.to_number();
            if i.is_finite() && i >= 0.0 && (i as usize) < arr.borrow().len() {
                return Value::bool(true);
            }
        }
        Value::bool(false)
    }));
    let from_entries = Value::native(Arc::new(|args, vm| {
        let iter = args.first().cloned().unwrap_or(Value::undefined());
        if iter.is_null() || iter.is_undefined() {
            vm.throw_exception(Value::string(
                "TypeError: Object.fromEntries requires an iterable".to_string(),
            ));
            return Value::undefined();
        }
        let items: Vec<Value> = if let Some(arr) = iter.as_array() {
            arr.borrow().to_values()
        } else {
            Vec::new()
        };
        let mut props = HashMap::new();
        for item in items {
            if let Some(arr) = item.as_array() {
                let arr = arr.borrow();
                if arr.len() > 0 {
                    let k = arr.get(0);
                    let v = if arr.len() > 1 {
                        arr.get(1)
                    } else {
                        Value::undefined()
                    };
                    if let Some(id) = k.as_symbol() {
                        props.insert(format!("\0sym_{}", id), v);
                    } else {
                        let key = match k.as_str() {
                            Some(s) => s.to_string(),
                            None => format!("{}", k),
                        };
                        props.insert(key, v);
                    }
                }
            }
        }
        Value::object(props)
    }));
    let mut m = HashMap::new();
    m.insert("keys".to_string(), keys);
    m.insert("values".to_string(), values);
    m.insert("entries".to_string(), entries);
    m.insert("hasOwn".to_string(), has_own);
    m.insert("fromEntries".to_string(), from_entries);
    Value::object(m)
}

/// `Array` global: `Array.isArray`, `Array.from` (array-likes, strings, and
/// Map/Set), `Array.of`.
pub(crate) fn make_array_module() -> Value {
    let is_array = Value::native(Arc::new(|args, _vm| {
        Value::bool(args.first().map(|v| v.is_array()).unwrap_or(false))
    }));
    let from = Value::native(Arc::new(|args, vm| {
        let src = args.first().cloned().unwrap_or(Value::undefined());
        let map_fn = args
            .get(1)
            .cloned()
            .filter(|v| v.is_function() || v.is_native());
        let mut out: Vec<Value> = Vec::new();
        if let Some(ad) = src.as_array() {
            out = ad.borrow().to_values();
        } else if let Some(s) = src.as_str() {
            out = s.chars().map(|c| Value::string(c.to_string())).collect();
        } else if let Some(m) = src.as_object() {
            let container = m.borrow().container;
            if container == 1 {
                for (k, v) in container_pairs(&src) {
                    out.push(Value::array(vec![k, v]));
                }
            } else if container == 2 {
                for (_, v) in container_pairs(&src) {
                    out.push(v);
                }
            } else if container == 0 {
                // Array-like: numeric `length`, indexed reads.
                let m = src.as_object().unwrap();
                let len = {
                    let m = m.borrow();
                    m.get("length")
                        .map(|v| {
                            let n = v.to_number();
                            if n.is_nan() {
                                0
                            } else {
                                n.trunc().max(0.0) as usize
                            }
                        })
                        .unwrap_or(0)
                };
                for i in 0..len {
                    let m = m.borrow();
                    let v = m.get(&i.to_string()).cloned().unwrap_or(Value::undefined());
                    drop(m);
                    out.push(v);
                }
            }
        }
        if let Some(f) = map_fn {
            for i in 0..out.len() {
                let e = out[i].clone();
                out[i] = vm.call_value(&f, &[e, Value::int(i as i64)]);
            }
        }
        Value::array(out)
    }));
    let of = Value::native(Arc::new(|args, _vm| Value::array(args.to_vec())));
    let mut m = HashMap::new();
    m.insert("isArray".to_string(), is_array);
    m.insert("from".to_string(), from);
    m.insert("of".to_string(), of);
    Value::object(m)
}
