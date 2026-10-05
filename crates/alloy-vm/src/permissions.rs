/// Granular runtime permissions / capability sandbox for Alloy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub allow_all: bool,
    pub allow_read: bool,
    pub allow_write: bool,
    pub allow_net: bool,
    pub allow_python: bool,
    pub allow_spawn: bool,
    pub read_whitelist: Vec<String>,
    pub write_whitelist: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            allow_all: true,
            allow_read: true,
            allow_write: true,
            allow_net: true,
            allow_python: true,
            allow_spawn: true,
            read_whitelist: Vec::new(),
            write_whitelist: Vec::new(),
        }
    }
}

impl Permissions {
    pub fn unrestricted() -> Self {
        Self::default()
    }

    pub fn sandboxed() -> Self {
        Self {
            allow_all: false,
            allow_read: false,
            allow_write: false,
            allow_net: false,
            allow_python: false,
            allow_spawn: false,
            read_whitelist: Vec::new(),
            write_whitelist: Vec::new(),
        }
    }

    pub fn allow_all(mut self, allow: bool) -> Self {
        self.allow_all = allow;
        if allow {
            self.allow_read = true;
            self.allow_write = true;
            self.allow_net = true;
            self.allow_python = true;
            self.allow_spawn = true;
        }
        self
    }

    pub fn allow_read(mut self, allow: bool) -> Self {
        self.allow_read = allow;
        self
    }

    pub fn allow_write(mut self, allow: bool) -> Self {
        self.allow_write = allow;
        self
    }

    pub fn allow_net(mut self, allow: bool) -> Self {
        self.allow_net = allow;
        self
    }

    pub fn allow_python(mut self, allow: bool) -> Self {
        self.allow_python = allow;
        self
    }

    pub fn allow_spawn(mut self, allow: bool) -> Self {
        self.allow_spawn = allow;
        self
    }

    pub fn check_read(&self, path: &str) -> Result<(), String> {
        if self.allow_all || self.allow_read {
            return Ok(());
        }
        if self
            .read_whitelist
            .iter()
            .any(|allowed| path.starts_with(allowed))
        {
            return Ok(());
        }
        Err(format!(
            "PermissionDenied: read access to '{}' is not permitted",
            path
        ))
    }

    pub fn check_write(&self, path: &str) -> Result<(), String> {
        if self.allow_all || self.allow_write {
            return Ok(());
        }
        if self
            .write_whitelist
            .iter()
            .any(|allowed| path.starts_with(allowed))
        {
            return Ok(());
        }
        Err(format!(
            "PermissionDenied: write access to '{}' is not permitted",
            path
        ))
    }

    pub fn check_net(&self, target: &str) -> Result<(), String> {
        if self.allow_all || self.allow_net {
            return Ok(());
        }
        Err(format!(
            "PermissionDenied: network access to '{}' is not permitted",
            target
        ))
    }

    pub fn check_python(&self) -> Result<(), String> {
        if self.allow_all || self.allow_python {
            return Ok(());
        }
        Err("PermissionDenied: Python access is not permitted".to_string())
    }

    pub fn check_spawn(&self) -> Result<(), String> {
        if self.allow_all || self.allow_spawn {
            return Ok(());
        }
        Err("PermissionDenied: spawn access is not permitted".to_string())
    }
}
