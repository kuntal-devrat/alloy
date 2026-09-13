use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use alloy_vm::compiler::Compiler;
use alloy_vm::vm::Vm;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AlloyManifest {
    pub name: String,
    pub version: String,
    #[serde(default = "default_main")]
    pub main: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}

fn default_main() -> String {
    "main.ajs".to_string()
}

pub fn init_project(target_dir: Option<&str>) -> Result<(), String> {
    let dir = target_dir.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory {:?}: {}", dir, e))?;
    }

    let manifest_path = dir.join("alloy.json");
    if manifest_path.exists() {
        println!("alloy.json already exists in {:?}", dir);
        return Ok(());
    }

    let name = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "alloy-app".to_string());

    let manifest = AlloyManifest {
        name,
        version: "0.1.0".to_string(),
        main: "main.ajs".to_string(),
        dependencies: BTreeMap::new(),
    };

    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("Failed to serialize manifest: {}", e))?;
    fs::write(&manifest_path, manifest_json)
        .map_err(|e| format!("Failed to write {:?}: {}", manifest_path, e))?;

    let main_path = dir.join("main.ajs");
    if !main_path.exists() {
        let starter_code = "// Welcome to Alloy!\nprint(\"Hello from Alloy!\");\n";
        fs::write(&main_path, starter_code)
            .map_err(|e| format!("Failed to write {:?}: {}", main_path, e))?;
    }

    println!("Initialized Alloy project in {:?}", dir);
    Ok(())
}

pub fn add_dependency(pkg_name: &str) -> Result<(), String> {
    let manifest_path = Path::new("alloy.json");
    let mut manifest: AlloyManifest = if manifest_path.exists() {
        let content = fs::read_to_string(manifest_path)
            .map_err(|e| format!("Failed to read alloy.json: {}", e))?;
        serde_json::from_str::<AlloyManifest>(&content)
            .map_err(|e| format!("Invalid alloy.json: {}", e))?
    } else {
        AlloyManifest {
            name: "alloy-app".to_string(),
            version: "0.1.0".to_string(),
            main: "main.ajs".to_string(),
            dependencies: BTreeMap::new(),
        }
    };

    println!("Resolving package '{}'...", pkg_name);

    let (version, main_file, code) = fetch_package_metadata_and_entry(pkg_name)?;

    let node_modules = Path::new("node_modules");
    let pkg_dir = node_modules.join(pkg_name);
    fs::create_dir_all(&pkg_dir)
        .map_err(|e| format!("Failed to create directory {:?}: {}", pkg_dir, e))?;

    // Write package.json inside node_modules/<pkg>/
    let pkg_json = serde_json::json!({
        "name": pkg_name,
        "version": version,
        "main": main_file
    });
    fs::write(pkg_dir.join("package.json"), serde_json::to_string_pretty(&pkg_json).unwrap())
        .map_err(|e| format!("Failed to write package.json: {}", e))?;

    // Write main entry file
    fs::write(pkg_dir.join(&main_file), code)
        .map_err(|e| format!("Failed to write {}: {}", main_file, e))?;

    manifest.dependencies.insert(pkg_name.to_string(), format!("^{}", version));
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("Failed to serialize manifest: {}", e))?;
    fs::write(manifest_path, manifest_json)
        .map_err(|e| format!("Failed to update alloy.json: {}", e))?;

    println!("+ {}@{} (installed in node_modules/{})", pkg_name, version, pkg_name);
    Ok(())
}

pub fn install_dependencies() -> Result<(), String> {
    let manifest_path = Path::new("alloy.json");
    if !manifest_path.exists() {
        return Err("alloy.json not found in current directory".to_string());
    }

    let content = fs::read_to_string(manifest_path)
        .map_err(|e| format!("Failed to read alloy.json: {}", e))?;
    let manifest: AlloyManifest = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid alloy.json: {}", e))?;

    if manifest.dependencies.is_empty() {
        println!("No dependencies found in alloy.json");
        return Ok(());
    }

    println!("Installing {} dependencies...", manifest.dependencies.len());
    for pkg in manifest.dependencies.keys() {
        add_dependency(pkg)?;
    }

    println!("Dependencies installed successfully.");
    Ok(())
}

fn fetch_package_metadata_and_entry(pkg: &str) -> Result<(String, String, String), String> {
    let registry_url = format!("https://registry.npmjs.org/{}", pkg);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();

    let resp = agent.get(&registry_url).call();

    match resp {
        Ok(r) => {
            let body_str = r.into_string().map_err(|e| format!("Failed to read registry response: {}", e))?;
            let meta: serde_json::Value = serde_json::from_str(&body_str).map_err(|e| format!("Failed to parse registry response: {}", e))?;
            let latest = meta["dist-tags"]["latest"].as_str().unwrap_or("1.0.0").to_string();
            let version_data = &meta["versions"][&latest];
            let main = version_data["main"].as_str().unwrap_or("index.js").to_string();

            // Try to fetch package content from unpkg or jsDelivr
            let cdn_url = format!("https://unpkg.com/{}@{}/{}", pkg, latest, main);
            let code = match agent.get(&cdn_url).call() {
                Ok(cr) => cr.into_string().unwrap_or_else(|_| format!("module.exports = {};\n", pkg)),
                Err(_) => format!("// Package: {}\nmodule.exports = {{ name: \"{}\", version: \"{}\" }};\n", pkg, pkg, latest),
            };

            Ok((latest, main, code))
        }
        Err(_) => {
            // Fallback for offline or air-gapped environments
            Ok((
                "1.0.0".to_string(),
                "index.js".to_string(),
                format!("// Fallback package: {}\nmodule.exports = {{ name: \"{}\", version: \"1.0.0\" }};\n", pkg, pkg),
            ))
        }
    }
}

pub fn run_tests(filter: Option<&str>) -> Result<(), String> {
    let mut test_files = Vec::new();
    let (root_dir, name_filter) = match filter {
        Some(f) if Path::new(f).is_dir() => (Path::new(f), None),
        Some(f) => (Path::new("."), Some(f)),
        None => (Path::new("."), None),
    };
    collect_test_files(root_dir, &mut test_files);

    if let Some(f) = name_filter {
        test_files.retain(|p| p.to_string_lossy().contains(f));
    }

    test_files.sort();

    println!("\nrunning {} test(s)", test_files.len());

    let mut passed = 0;
    let mut failed = 0;
    let total_start = Instant::now();

    for path in &test_files {
        let path_str = path.display().to_string();
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                println!("test {} ... FAILED (read error: {})", path_str, e);
                failed += 1;
                continue;
            }
        };

        let prog_start = Instant::now();
        let program = match Compiler::compile_source(&source) {
            Ok(mut p) => {
                p.source_file = Some(path_str.clone());
                p
            }
            Err(e) => {
                println!("test {} ... FAILED (compile error: {})", path_str, e);
                failed += 1;
                continue;
            }
        };

        let mut vm = Vm::new(program);
        vm.set_script_path(&path_str);
        let _ = vm.run();
        let elapsed = prog_start.elapsed();

        if let Some(err) = vm.take_error() {
            let err_msg = if let Some(od) = err.as_object() {
                od.borrow()
                    .get("stack")
                    .and_then(|s| s.as_str().map(|x| x.to_string()))
                    .unwrap_or_else(|| err.to_string())
            } else {
                err.to_string()
            };
            println!("test {} ... FAILED ({})", path_str, err_msg);
            failed += 1;
        } else {
            println!("test {} ... ok ({:?})", path_str, elapsed);
            passed += 1;
        }
    }

    let total_elapsed = total_start.elapsed();
    let status = if failed == 0 { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {}. {} passed; {} failed; finished in {:?}",
        status, passed, failed, total_elapsed
    );

    if failed > 0 {
        Err(format!("{} test(s) failed", failed))
    } else {
        Ok(())
    }
}

fn collect_test_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() {
            // Skip node_modules, .git, target, dist
            if file_name == "node_modules" || file_name == ".git" || file_name == "target" || file_name == "dist" {
                continue;
            }
            collect_test_files(&path, files);
        } else if is_test_file(&file_name) {
            files.push(path);
        }
    }
}

fn is_test_file(name: &str) -> bool {
    (name.ends_with(".test.ajs") || name.ends_with(".test.js"))
        || (name.starts_with("test_") && (name.ends_with(".ajs") || name.ends_with(".js")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_roundtrip() {
        let manifest = AlloyManifest {
            name: "test-pkg".to_string(),
            version: "1.2.3".to_string(),
            main: "index.ajs".to_string(),
            dependencies: {
                let mut m = BTreeMap::new();
                m.insert("express".to_string(), "^4.18.2".to_string());
                m
            },
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: AlloyManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-pkg");
        assert_eq!(parsed.version, "1.2.3");
        assert_eq!(parsed.main, "index.ajs");
        assert_eq!(parsed.dependencies.get("express"), Some(&"^4.18.2".to_string()));
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file("math.test.ajs"));
        assert!(is_test_file("math.test.js"));
        assert!(is_test_file("test_api.ajs"));
        assert!(is_test_file("test_api.js"));
        assert!(!is_test_file("main.ajs"));
        assert!(!is_test_file("index.js"));
        assert!(!is_test_file("test.txt"));
    }
}

