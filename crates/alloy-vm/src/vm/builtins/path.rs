use alloy_core::value::Value;
use hashbrown::HashMap;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR};
use std::sync::Arc;

/// Normalize a path string: collapse multiple separators, resolve `.` and `..`
/// segments cleanly across platforms.
pub(crate) fn normalize_path(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let path = Path::new(p);
    let mut components = Vec::new();
    let is_abs = path.is_absolute();

    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => {
                components.push(prefix.as_os_str().to_string_lossy().to_string());
            }
            Component::RootDir => {
                if components.is_empty() {
                    components.push(MAIN_SEPARATOR.to_string());
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = components.last() {
                    if last != &MAIN_SEPARATOR.to_string() && last != ".." {
                        components.pop();
                        continue;
                    }
                }
                if !is_abs {
                    components.push("..".to_string());
                }
            }
            Component::Normal(c) => {
                components.push(c.to_string_lossy().to_string());
            }
        }
    }

    if components.is_empty() {
        return if is_abs {
            MAIN_SEPARATOR.to_string()
        } else {
            ".".to_string()
        };
    }

    let mut result = String::new();
    for (i, c) in components.iter().enumerate() {
        if i > 0 && !result.ends_with(MAIN_SEPARATOR) && c != &MAIN_SEPARATOR.to_string() {
            result.push(MAIN_SEPARATOR);
        }
        result.push_str(c);
    }
    result
}

/// Node.js standard `path` module implementation for Alloy.
pub(crate) fn make_path_module() -> Value {
    let join = Value::native(Arc::new(|args, _vm| {
        let mut parts = Vec::new();
        for a in args {
            if let Some(s) = a.as_str() {
                if !s.is_empty() {
                    parts.push(s);
                }
            }
        }
        if parts.is_empty() {
            return Value::string(".".to_string());
        }
        let joined = parts.join(std::path::MAIN_SEPARATOR_STR);
        Value::string(normalize_path(&joined))
    }));

    let resolve = Value::native(Arc::new(|args, _vm| {
        let mut base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        for a in args {
            if let Some(s) = a.as_str() {
                let p = Path::new(s);
                if p.is_absolute() {
                    base = p.to_path_buf();
                } else {
                    base.push(p);
                }
            }
        }
        Value::string(normalize_path(&base.to_string_lossy()))
    }));

    let dirname = Value::native(Arc::new(|args, _vm| {
        let p_str = args.first().and_then(|v| v.as_str()).unwrap_or("");
        if p_str.is_empty() {
            return Value::string(".".to_string());
        }
        let p = Path::new(p_str);
        match p.parent() {
            Some(parent) => {
                let s = parent.to_string_lossy();
                if s.is_empty() {
                    Value::string(".".to_string())
                } else {
                    Value::string(s.into_owned())
                }
            }
            None => Value::string(p_str.to_string()),
        }
    }));

    let basename = Value::native(Arc::new(|args, _vm| {
        let p_str = args.first().and_then(|v| v.as_str()).unwrap_or("");
        let ext_opt = args.get(1).and_then(|v| v.as_str());
        let p = Path::new(p_str);
        let name = match p.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => p_str.to_string(),
        };
        let final_name = if let Some(ext) = ext_opt {
            if !ext.is_empty() && name.ends_with(ext) {
                name[..name.len() - ext.len()].to_string()
            } else {
                name
            }
        } else {
            name
        };
        Value::string(final_name)
    }));

    let extname = Value::native(Arc::new(|args, _vm| {
        let p_str = args.first().and_then(|v| v.as_str()).unwrap_or("");
        let p = Path::new(p_str);
        match p.extension() {
            Some(e) => Value::string(format!(".{}", e.to_string_lossy())),
            None => Value::string(String::new()),
        }
    }));

    let is_abs = Value::native(Arc::new(|args, _vm| {
        let p_str = args.first().and_then(|v| v.as_str()).unwrap_or("");
        Value::bool(Path::new(p_str).is_absolute())
    }));

    let normalize = Value::native(Arc::new(|args, _vm| {
        let p_str = args.first().and_then(|v| v.as_str()).unwrap_or("");
        Value::string(normalize_path(p_str))
    }));

    let mut m = HashMap::new();
    m.insert("join".to_string(), join);
    m.insert("resolve".to_string(), resolve);
    m.insert("dirname".to_string(), dirname);
    m.insert("basename".to_string(), basename);
    m.insert("extname".to_string(), extname);
    m.insert("isAbsolute".to_string(), is_abs);
    m.insert("normalize".to_string(), normalize);
    m.insert("sep".to_string(), Value::string(MAIN_SEPARATOR.to_string()));
    m.insert(
        "delimiter".to_string(),
        Value::string(if cfg!(windows) { ";" } else { ":" }.to_string()),
    );

    Value::object(m)
}
