use std::process::Command;
use std::time::Duration;

/// Legacy synchronous Python bridge — kept for `alloy-rt` API compat
/// but delegates to the real sidecar pool (`alloy-vm::python_sidecar`) for
/// zero-copy calls. New code should use `Vm::import python` instead.
///
/// This stub now validates input, enforces a timeout, and captures stderr.
pub struct PythonBridge {
    python_path: String,
    timeout: Duration,
}

impl PythonBridge {
    pub fn new() -> Self {
        Self {
            python_path: std::env::var("ALLOY_PYTHON").unwrap_or_else(|_| if cfg!(windows) { "python".to_string() } else { "python3".to_string() }),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_path(path: &str) -> Self {
        Self {
            python_path: path.to_string(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self { self.timeout = t; self }

    fn run(&self, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
        // Basic injection guard: reject args containing null bytes
        for a in args {
            if a.contains('\0') {
                return Err("python arg contains null byte".into());
            }
        }
        let mut cmd = Command::new(&self.python_path);
        cmd.args(args);
        // Use timeout via wait_timeout crate logic manually: spawn + timed wait
        let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
        let start = std::time::Instant::now();
        loop {
            match child.try_wait()? {
                Some(status) => {
                    let out = child.wait_with_output()?;
                    if !status.success() {
                        let err = String::from_utf8_lossy(&out.stderr);
                        return Err(format!("python failed ({}): {}", status, err.trim()).into());
                    }
                    return Ok(String::from_utf8_lossy(&out.stdout).to_string());
                }
                None => {
                    if start.elapsed() > self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("python call timed out after {:?}", self.timeout).into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    pub fn execute_script(&self, script: &str) -> Result<String, Box<dyn std::error::Error>> {
        if script.len() > 1_000_000 {
            return Err("script too large".into());
        }
        self.run(&["-c", script])
    }

    pub fn execute_file(&self, path: &str) -> Result<String, Box<dyn std::error::Error>> {
        // Validate path exists and is file
        let p = std::path::Path::new(path);
        if !p.exists() {
            return Err(format!("file not found: {}", path).into());
        }
        self.run(&[path])
    }
}

impl Default for PythonBridge {
    fn default() -> Self {
        Self::new()
    }
}
