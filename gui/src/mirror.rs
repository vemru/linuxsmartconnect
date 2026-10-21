//! Helper processes: the `smartconnect-mirror` viewer and `smartconnect-crosscontrol`, each run
//! as a child process and stopped with SIGINT so it can tell the phone to stop first.

use std::path::PathBuf;
use std::process::{Child, Command};

pub struct Mirror {
    /// Binary name, e.g. `smartconnect-mirror`.
    name: &'static str,
    /// Shown in failure toasts, e.g. "Mirroring".
    label: &'static str,
    child: Option<Child>,
    stop_requested: bool,
}

pub enum Exit {
    /// Stopped by us or exited cleanly.
    Clean,
    Failed(String),
}

/// Where a helper is, in the same order smartconnectd uses: `$SMARTCONNECT_<NAME>_BIN` (e.g.
/// `SMARTCONNECT_DISPLAY_BIN` for `smartconnect-display`), else a file of that name next to this
/// program (an install puts all binaries in one directory, and so does a workspace build), else
/// the bare name, which `Command` looks up on `$PATH`.
pub fn binary(name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_var(name)).filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The override variable for a helper: `smartconnect-display` → `SMARTCONNECT_DISPLAY_BIN`.
pub fn env_var(name: &str) -> String {
    format!("SMARTCONNECT_{}_BIN", name.trim_start_matches("smartconnect-").to_uppercase().replace('-', "_"))
}

impl Mirror {
    /// The screen-mirroring viewer.
    pub fn viewer() -> Self {
        Self { name: "smartconnect-mirror", label: "Mirroring", child: None, stop_requested: false }
    }

    /// Cross Control (PC pointer/keyboard drives the phone).
    pub fn cross_control() -> Self {
        Self { name: "smartconnect-crosscontrol", label: "Cross Control", child: None, stop_requested: false }
    }

    pub fn running(&self) -> bool {
        self.child.is_some()
    }

    /// SIGINT sent, child not reaped yet.
    pub fn stopping(&self) -> bool {
        self.child.is_some() && self.stop_requested
    }

    pub fn start(&mut self, args: &[&str]) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        let bin = binary(self.name);
        let child = Command::new(&bin).args(args).spawn().map_err(|e| format!("Could not start {}: {e}", bin.display()))?;
        self.child = Some(child);
        self.stop_requested = false;
        Ok(())
    }

    /// SIGINT: the helper stops the phone's stream and exits on its own.
    pub fn stop(&mut self) {
        if let Some(child) = &self.child {
            self.stop_requested = true;
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: plain kill(2) on our own (not yet reaped) child.
                unsafe { libc::kill(pid, libc::SIGINT) };
            }
        }
    }

    /// Non-blocking reap; returns how the child ended once it has.
    pub fn poll(&mut self) -> Option<Exit> {
        let child = self.child.as_mut()?;
        let status = match child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => Some(status),
            Err(_) => None,
        };
        self.child = None;
        Some(match status {
            _ if self.stop_requested => Exit::Clean,
            Some(s) if s.success() => Exit::Clean,
            Some(s) => Exit::Failed(format!("{} stopped ({s})", self.label)),
            None => Exit::Failed(format!("{} stopped", self.label)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_lookup_order() {
        assert_eq!(env_var("smartconnect-mirror"), "SMARTCONNECT_MIRROR_BIN");
        assert_eq!(env_var("smartconnect-crosscontrol"), "SMARTCONNECT_CROSSCONTROL_BIN");
        // No override and nothing next to the test binary: left to the $PATH search.
        let name = "smartconnect-no-such-helper";
        assert_eq!(binary(name), PathBuf::from(name));
        // A file next to the running executable is used.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(binary(exe.file_name().unwrap().to_str().unwrap()), exe);
        // The override wins. SAFETY: test-only variable that no other test reads.
        unsafe { std::env::set_var("SMARTCONNECT_LOOKUP_TEST_BIN", "/x/y") };
        assert_eq!(binary("smartconnect-lookup-test"), PathBuf::from("/x/y"));
        unsafe { std::env::remove_var("SMARTCONNECT_LOOKUP_TEST_BIN") };
    }
}
