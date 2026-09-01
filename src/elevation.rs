//! Best-effort detection of whether wgtui runs elevated (as Administrator).
//!
//! `winget install --scope machine` (wgtui's default) needs admin rights, so a
//! non-elevated session gets a status-bar warning.

use std::process::{Command, Stdio};

/// Whether the current process has administrative rights.
///
/// Uses `net session`, which requires elevation and is a built-in with no side
/// effects. Any failure to run it is treated as "not elevated".
#[must_use]
pub fn is_elevated() -> bool {
    Command::new("net")
        .arg("session")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The short status-bar marker to show, or `None` when elevated.
///
/// (Kept short so it fits next to `q quit`; the README explains the impact —
/// `--scope machine` installs need admin, use `"scope": "user"` otherwise.)
#[must_use]
pub fn elevation_warning(elevated: bool) -> Option<&'static str> {
    if elevated { None } else { Some(" not admin ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_shown_only_when_not_elevated() {
        assert!(elevation_warning(true).is_none());
        let w = elevation_warning(false).expect("warning when not elevated");
        assert!(w.to_lowercase().contains("admin"));
    }

    #[test]
    fn is_elevated_does_not_panic() {
        let _ = is_elevated();
    }
}
