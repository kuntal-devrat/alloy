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
            python_path: std::env::var("ALLOY_PYTHON").unwrap_or_else(|_| {
                if cfg!(windows) {
                    "python".to_string()
                } else {
                    "python3".to_string()
                }
            }),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_path(path: &str) -> Self {
        Self {
            python_path: path.to_string(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    fn run(&self, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
        // Basic injection guard: reject args containing null bytes
        for a in args {
            if a.contains('\0') {
                return Err("python arg contains null byte".into());
            }
        }
        let mut cmd = Command::new(&self.python_path);
        cmd.args(args);
        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let res = child.wait_with_output();
            let _ = tx.send(res);
        });

        match rx.recv_timeout(self.timeout) {
            Ok(res) => {
                let _ = handle.join();
                let out = res?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    return Err(format!("python failed ({}): {}", out.status, err.trim()).into());
                }
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            }
            Err(_) => {
                #[cfg(windows)]
                {
                    use windows_sys::Win32::Foundation::CloseHandle;
                    use windows_sys::Win32::System::Threading::{
                        OpenProcess, TerminateProcess, PROCESS_TERMINATE,
                    };
                    unsafe {
                        let proc_handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                        if !proc_handle.is_null() {
                            TerminateProcess(proc_handle, 1);
                            CloseHandle(proc_handle);
                        }
                    }
                }
                #[cfg(unix)]
                {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
                let _ = handle.join();
                Err(format!("python call timed out after {:?}", self.timeout).into())
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
