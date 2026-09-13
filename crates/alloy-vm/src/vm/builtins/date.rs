use std::sync::Arc;
use alloy_core::value::{to_string_js, Value, VmHost};


pub(crate) fn date_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// A Date instance: an object with `container = DATE_CONTAINER` holding epoch
/// ms under the reserved `DATE_MS_KEY` property, proto = Date.prototype. The
/// value-layer coercion hooks (value.rs) read that key, so `+d`, `d - d`,
/// `String(d)`, and `d == "Wed …"` behave like Node with no VM involvement.
pub(crate) fn date_instance(ms: f64, proto: Value) -> Value {
    let v = Value::object_with_proto(proto);
    if let Some(od) = v.as_object() {
        let mut od = od.borrow_mut();
        od.container = alloy_core::value::DATE_CONTAINER;
        od.set(alloy_core::value::DATE_MS_KEY, Value::number(ms));
    }
    v
}

/// Epoch ms a `new Date(args…)` / `Date(args…)` call resolves to, with JS
/// argument semantics: no args → now; one number/Date → that time; one
/// string → `Date.parse`; one undefined → Invalid; two or more → local
/// components (0-99 year → 1900+).
pub(crate) fn date_ctor_ms(args: &[Value]) -> f64 {
    match args.len() {
        0 => date_now_ms(),
        1 => {
            let a = &args[0];
            if a.is_undefined() {
                f64::NAN
            } else if let Some(ms) = alloy_core::value::date_ms(a) {
                ms
            } else if a.is_string() {
                alloy_core::value::date_parse(a.as_str().unwrap_or(""))
            } else {
                // null → 0, true → 1, objects/arrays → NaN (ToNumber).
                a.to_number()
            }
        }
        _ => {
            let get = |i: usize| -> f64 { args.get(i).map(|v| v.to_number()).unwrap_or(0.0) };
            let mut y = get(0);
            let (mo, d, h, mi, s, ms) = (get(1), get(2), get(3), get(4), get(5), get(6));
            if [y, mo, d, h, mi, s, ms]
                .iter()
                .any(|v| !v.is_finite())
            {
                return f64::NAN;
            }
            if (0.0..=99.0).contains(&y) {
                y += 1900.0;
            }
            alloy_core::value::ms_from_local_components(
                y as i64,
                mo as i64,
                d as i64,
                h as i64,
                mi as i64,
                s as i64,
                ms as i64,
            )
        }
    }
}

/// The receiver's stored epoch ms for a Date-prototype native; `NaN` when
/// the receiver isn't a Date instance or is an Invalid Date.
pub(crate) fn this_date_ms(vm: &dyn VmHost) -> f64 {
    alloy_core::value::date_ms(&vm.this_value()).unwrap_or(f64::NAN)
}

/// Component getters: `getFullYear`…`getMilliseconds` (local or UTC) plus
/// `getDay` (weekday) and `getTimezoneOffset`. All return NaN on Invalid or
/// a non-Date receiver (the engine's non-throwing style).
pub(crate) fn date_getter_native(comp: &str, utc: bool) -> Value {
    let comp = comp.to_string();
    Value::native(Arc::new(move |_args, vm| {
        let ms = this_date_ms(vm);
        let (y, m, d, h, mi, s, msp) = if utc {
            alloy_core::value::ms_components_utc(ms)
        } else {
            alloy_core::value::ms_components_local(ms)
        };
        let out: f64 = match comp.as_str() {
            "year" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    y as f64
                }
            }
            "month" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    (m - 1) as f64
                }
            }
            "date" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    d as f64
                }
            }
            "day" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    ((ms.floor() as i64).div_euclid(86_400_000).rem_euclid(7) + 4).rem_euclid(7)
                        as f64
                }
            }
            "hours" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    h as f64
                }
            }
            "minutes" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    mi as f64
                }
            }
            "seconds" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    s as f64
                }
            }
            "milliseconds" => {
                if !ms.is_finite() {
                    f64::NAN
                } else {
                    msp as f64
                }
            }
            _ => f64::NAN,
        };
        Value::number(out)
    }))
}

/// Which local/UTC components a setter replaces, in argument order.
const DATE_SET_ORDER: &[&[&str]] = &[
    &["milliseconds"],          // setMilliseconds(ms)
    &["seconds", "milliseconds"], // setSeconds(s, ms)
    &["minutes", "seconds", "milliseconds"], // setMinutes(mi, s, ms)
    &["hours", "minutes", "seconds", "milliseconds"], // setHours(h, mi, s, ms)
    &["date"],                 // setDate(d)
    &["month", "date"],       // setMonth(mo, d)
    &["year", "month", "date"], // setFullYear(y, mo, d)
];

/// Component setters: recompute the stored time from the current components
/// with the named ones replaced (missing/undefined args keep the current
/// value; a NaN component makes the Date invalid). Returns the new ms.
pub(crate) fn date_setter_native(order: &[&str], utc: bool) -> Value {
    let order: Vec<String> = order.iter().map(|s| s.to_string()).collect();
    Value::native(Arc::new(move |args, vm| {
        let this = vm.this_value();
        let cur = this_date_ms(vm);
        let (mut y, mut m, mut d, mut h, mut mi, mut s, mut msp) = if utc {
            alloy_core::value::ms_components_utc(cur)
        } else {
            alloy_core::value::ms_components_local(cur)
        };
        let mut invalid = !cur.is_finite();
        for (i, comp) in order.iter().enumerate() {
            let Some(v) = args.get(i) else { continue };
            if v.is_undefined() {
                continue;
            }
            let n = v.to_number();
            if n.is_nan() || n.is_infinite() {
                invalid = true;
                continue;
            }
            let n = n as i64;
            match comp.as_str() {
                "year" => y = if (0..=99).contains(&n) { n + 1900 } else { n },
                "month" => m = n + 1,
                "date" => d = n,
                "hours" => h = n,
                "minutes" => mi = n,
                "seconds" => s = n,
                "milliseconds" => msp = n,
                _ => {}
            }
        }
        let new_ms = if invalid {
            f64::NAN
        } else if utc {
            alloy_core::value::ms_from_utc_components(y, m - 1, d, h, mi, s, msp)
        } else {
            alloy_core::value::ms_from_local_components(y, m - 1, d, h, mi, s, msp)
        };
        alloy_core::value::date_set_ms(&this, new_ms);
        Value::number(new_ms)
    }))
}

/// One shared Date method native installed on Date.prototype; the receiver
/// comes from `this` like the Map/Set methods.
pub(crate) fn date_method_native(name: &str) -> Value {
    match name {
        "getTime" | "valueOf" => Value::native(Arc::new(|_args, vm| {
            Value::number(this_date_ms(vm))
        })),
        "getTimezoneOffset" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::number(
                -(alloy_core::value::local_offset_ms(ms) as f64) / 60_000.0,
            )
        })),
        "getFullYear" => date_getter_native("year", false),
        "getMonth" => date_getter_native("month", false),
        "getDate" => date_getter_native("date", false),
        "getDay" => date_getter_native("day", false),
        "getHours" => date_getter_native("hours", false),
        "getMinutes" => date_getter_native("minutes", false),
        "getSeconds" => date_getter_native("seconds", false),
        "getMilliseconds" => date_getter_native("milliseconds", false),
        "getUTCFullYear" => date_getter_native("year", true),
        "getUTCMonth" => date_getter_native("month", true),
        "getUTCDate" => date_getter_native("date", true),
        "getUTCDay" => date_getter_native("day", true),
        "getUTCHours" => date_getter_native("hours", true),
        "getUTCMinutes" => date_getter_native("minutes", true),
        "getUTCSeconds" => date_getter_native("seconds", true),
        "getUTCMilliseconds" => date_getter_native("milliseconds", true),
        "setTime" => Value::native(Arc::new(|args, vm| {
            let this = vm.this_value();
            let n = args.first().map(|v| v.to_number()).unwrap_or(f64::NAN);
            alloy_core::value::date_set_ms(&this, n);
            Value::number(n)
        })),
        "setMilliseconds" => date_setter_native(DATE_SET_ORDER[0], false),
        "setSeconds" => date_setter_native(DATE_SET_ORDER[1], false),
        "setMinutes" => date_setter_native(DATE_SET_ORDER[2], false),
        "setHours" => date_setter_native(DATE_SET_ORDER[3], false),
        "setDate" => date_setter_native(DATE_SET_ORDER[4], false),
        "setMonth" => date_setter_native(DATE_SET_ORDER[5], false),
        "setFullYear" => date_setter_native(DATE_SET_ORDER[6], false),
        "setUTCMilliseconds" => date_setter_native(DATE_SET_ORDER[0], true),
        "setUTCSeconds" => date_setter_native(DATE_SET_ORDER[1], true),
        "setUTCMinutes" => date_setter_native(DATE_SET_ORDER[2], true),
        "setUTCHours" => date_setter_native(DATE_SET_ORDER[3], true),
        "setUTCDate" => date_setter_native(DATE_SET_ORDER[4], true),
        "setUTCMonth" => date_setter_native(DATE_SET_ORDER[5], true),
        "setUTCFullYear" => date_setter_native(DATE_SET_ORDER[6], true),
        "toString" => Value::native(Arc::new(|_args, vm| {
            Value::string(alloy_core::value::date_to_string(this_date_ms(vm)))
        })),
        "toISOString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            if !ms.is_finite() {
                vm.throw_exception(Value::string(
                    "RangeError: Invalid time value".to_string(),
                ));
                return Value::undefined();
            }
            Value::string(alloy_core::value::date_to_iso_string(ms))
        })),
        "toUTCString" | "toGMTString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_utc_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toDateString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_date_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toTimeString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_time_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                format!(
                    "{}, {}",
                    alloy_core::value::date_to_locale_date_string(ms),
                    alloy_core::value::date_to_locale_time_string(ms)
                )
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleDateString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_locale_date_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toLocaleTimeString" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            Value::string(if ms.is_finite() {
                alloy_core::value::date_to_locale_time_string(ms)
            } else {
                "Invalid Date".to_string()
            })
        })),
        "toJSON" => Value::native(Arc::new(|_args, vm| {
            let ms = this_date_ms(vm);
            if !ms.is_finite() {
                return Value::null();
            }
            Value::string(alloy_core::value::date_to_iso_string(ms))
        })),
        _ => Value::undefined(),
    }
}

/// Full `Date`: a callable constructor with the prototype carrying all
/// getters/setters/formatters and statics `now`/`parse`/`UTC` on the
/// constructor itself.
pub(crate) fn make_date_ctor() -> Value {
    let proto = Value::object_with_proto(Value::undefined());
    {
        let od = proto.as_object().unwrap();
        let mut od = od.borrow_mut();
        for name in [
            "getTime", "getFullYear", "getMonth", "getDate", "getDay", "getHours",
            "getMinutes", "getSeconds", "getMilliseconds", "getTimezoneOffset",
            "getUTCFullYear", "getUTCMonth", "getUTCDate", "getUTCDay", "getUTCHours",
            "getUTCMinutes", "getUTCSeconds", "getUTCMilliseconds", "setTime",
            "setMilliseconds", "setSeconds", "setMinutes", "setHours", "setDate",
            "setMonth", "setFullYear", "setUTCMilliseconds", "setUTCSeconds",
            "setUTCMinutes", "setUTCHours", "setUTCDate", "setUTCMonth", "setUTCFullYear",
            "toString", "toISOString", "toUTCString", "toGMTString", "toDateString",
            "toTimeString", "toLocaleString", "toLocaleDateString", "toLocaleTimeString",
            "toJSON", "valueOf",
        ] {
            od.set(name, date_method_native(name));
        }
    }
    let now = Value::native(Arc::new(|_args, _vm| Value::number(date_now_ms())));
    let parse = Value::native(Arc::new(|args, _vm| {
        let s = args.first().map(to_string_js).unwrap_or_default();
        Value::number(alloy_core::value::date_parse(&s))
    }));
    let utc = Value::native(Arc::new(|args, _vm| {
        let get = |i: usize| -> f64 { args.get(i).map(|v| v.to_number()).unwrap_or(0.0) };
        let mut y = get(0);
        let (mo, d, h, mi, s, ms) = (get(1), get(2), get(3), get(4), get(5), get(6));
        if [y, mo, d, h, mi, s, ms]
            .iter()
            .any(|v| !v.is_finite())
        {
            return Value::number(f64::NAN);
        }
        if (0.0..=99.0).contains(&y) {
            y += 1900.0;
        }
        Value::number(alloy_core::value::ms_from_utc_components(
            y as i64,
            mo as i64,
            d as i64,
            h as i64,
            mi as i64,
            s as i64,
            ms as i64,
        ))
    }));
    let ctor_proto = proto.clone();
    let ctor = Arc::new(move |args: &[Value], _vm: &mut dyn VmHost| {
        date_instance(date_ctor_ms(args), ctor_proto.clone())
    });
    Value::native_with_props(
        ctor,
        proto,
        vec![
            ("now".to_string(), now),
            ("parse".to_string(), parse),
            ("UTC".to_string(), utc),
        ],
    )
}
