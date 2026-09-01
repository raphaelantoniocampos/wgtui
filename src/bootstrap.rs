//! First-run winget bootstrap.
//!
//! When `winget` is not on `PATH`, wgtui offers to install the winget client
//! via PowerShell instead of just bailing out.

use std::io::{self, Write};
use std::process::Command;

/// PowerShell command that installs and repairs the winget client for the
/// current user.
pub const BOOTSTRAP_PS: &str = "Install-Module Microsoft.WinGet.Client -Force -Confirm:$false \
     -Scope CurrentUser; Repair-WinGetPackageManager";

/// Result of [`ensure_winget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootstrap {
    /// winget is usable (already was, or the install succeeded).
    Ready,
    /// winget is still missing; the caller should print guidance and exit.
    Unavailable,
}

/// Decides whether winget is ready, prompting for and running the bootstrap if
/// it is missing.
///
/// The three effects are injected so the decision logic is testable:
/// * `check` — is winget available now?
/// * `confirm` — ask the user; `true` means "go ahead".
/// * `run_bootstrap` — perform the install (return value currently unused; the
///   post-install `check` is authoritative).
pub fn ensure_winget(
    mut check: impl FnMut() -> bool,
    mut confirm: impl FnMut() -> bool,
    mut run_bootstrap: impl FnMut() -> bool,
) -> Bootstrap {
    if check() {
        return Bootstrap::Ready;
    }
    if !confirm() {
        return Bootstrap::Unavailable;
    }
    run_bootstrap();
    if check() {
        Bootstrap::Ready
    } else {
        Bootstrap::Unavailable
    }
}

/// Whether `input` is an affirmative answer (pt/en, case-insensitive).
fn parse_yes(input: &str) -> bool {
    matches!(
        input.trim().to_lowercase().as_str(),
        "s" | "sim" | "y" | "yes"
    )
}

/// Prompts on stderr/stdin for a yes/no answer. Defaults to no (including on
/// any read error, e.g. a non-interactive stdin).
pub fn prompt_yes_no(question: &str) -> bool {
    eprint!("{question} [s/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => parse_yes(&line),
    }
}

/// Runs the PowerShell bootstrap with inherited stdio so the user sees output.
pub fn run_powershell_bootstrap() -> bool {
    eprintln!("\n> powershell {BOOTSTRAP_PS}\n");
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            BOOTSTRAP_PS,
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Lines telling the user how to install winget by hand.
pub fn manual_instructions() -> [&'static str; 3] {
    [
        "winget continua indisponível. Instale manualmente e rode o wgtui de novo:",
        "  Install-Module Microsoft.WinGet.Client -Force -Scope CurrentUser",
        "  Repair-WinGetPackageManager",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_when_winget_already_present() {
        let mut confirmed = false;
        let r = ensure_winget(
            || true,
            || {
                confirmed = true;
                true
            },
            || true,
        );
        assert_eq!(r, Bootstrap::Ready);
        assert!(!confirmed, "must not prompt when winget is already there");
    }

    #[test]
    fn unavailable_when_user_declines() {
        let mut ran = false;
        let r = ensure_winget(
            || false,
            || false,
            || {
                ran = true;
                true
            },
        );
        assert_eq!(r, Bootstrap::Unavailable);
        assert!(!ran, "must not run bootstrap when the user says no");
    }

    #[test]
    fn ready_after_successful_bootstrap() {
        let mut checks = 0;
        let mut ran = 0;
        let r = ensure_winget(
            || {
                checks += 1;
                checks > 1 // missing first, present after install
            },
            || true,
            || {
                ran += 1;
                true
            },
        );
        assert_eq!(r, Bootstrap::Ready);
        assert_eq!(ran, 1);
        assert_eq!(checks, 2);
    }

    #[test]
    fn unavailable_when_bootstrap_does_not_fix_it() {
        let r = ensure_winget(|| false, || true, || false);
        assert_eq!(r, Bootstrap::Unavailable);
    }

    #[test]
    fn parse_yes_accepts_pt_and_en() {
        for y in ["s", "S", " sim ", "y", "YES"] {
            assert!(parse_yes(y), "{y:?} should be yes");
        }
        for n in ["", "n", "nao", "later", "ss"] {
            assert!(!parse_yes(n), "{n:?} should be no");
        }
    }
}
