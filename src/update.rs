//! Self-update: look for a newer GitHub release, download it, and swap it in
//! for the running executable.
//!
//! Network access shells out to `curl.exe` (bundled with Windows 10 1803+),
//! the same way the rest of the app shells out to `winget`/`powershell` —
//! no HTTP dependency. The decision logic (version comparison, URL parsing,
//! download validation, the exe swap) is pure and unit-tested; only
//! `latest_release_tag` and `download_release` touch the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const REPO: &str = "raphaelantoniocampos/wgtui";

/// Set to any value to skip the startup update check (offline machines, CI).
pub const DISABLE_ENV: &str = "WGTUI_NO_UPDATE_CHECK";

/// Parses `v0.2.1` / `0.2.1` (optionally with a `-pre`/`+build` suffix) into
/// `(major, minor, patch)`. Anything else is `None`.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `latest` is strictly newer than `current`. Unparseable input on
/// either side is "not newer" — never nag on something we can't read.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// Extracts the tag from the URL GitHub redirects `/releases/latest` to
/// (`https://github.com/<repo>/releases/tag/v0.2.1`).
pub fn tag_from_release_url(url: &str) -> Option<String> {
    let tag = url.trim().split("/releases/tag/").nth(1)?;
    let tag = tag.split(['?', '#', '/']).next()?.trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

/// Smallest plausible size for the real `wgtui.exe` (it's megabytes); anything
/// smaller is an error page or a truncated download.
const MIN_EXE_BYTES: u64 = 100 * 1024;

/// Rejects a download that can't be a Windows executable (an HTML error
/// page, a truncated file) so it never replaces the working exe.
pub fn validate_download(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let len = std::fs::metadata(path)
        .map_err(|e| format!("downloaded file is missing: {e}"))?
        .len();
    if len < MIN_EXE_BYTES {
        return Err(format!(
            "downloaded file is only {len} bytes — not wgtui.exe"
        ));
    }
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| format!("couldn't read downloaded file: {e}"))?;
    if &magic != b"MZ" {
        return Err("downloaded file is not a Windows executable".into());
    }
    Ok(())
}

/// `<path>` with `suffix` appended to its file name (`wgtui.exe` ->
/// `wgtui.exe.old`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Replaces `current` with `new`. A running exe can be renamed but not
/// overwritten on Windows, so the old file moves to `<current>.old` first;
/// if putting `new` in place fails, the old file is moved back.
pub fn swap_executable(current: &Path, new: &Path) -> std::io::Result<()> {
    let old = sibling(current, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old)?;
    if let Err(e) = std::fs::rename(new, current) {
        let _ = std::fs::rename(&old, current);
        return Err(e);
    }
    Ok(())
}

/// Best-effort removal of the `.old` file a previous update left behind.
pub fn cleanup_old_executable() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
    }
}

fn curl() -> Command {
    let mut c = Command::new("curl.exe");
    c.stdin(Stdio::null()).stderr(Stdio::null());
    c
}

/// The newest release tag on GitHub, or `None` offline / on any failure.
///
/// Follows the `/releases/latest` redirect and reads the tag off the final
/// URL: no JSON, and none of the API's unauthenticated rate limit. (GitHub's
/// "latest" already skips drafts and pre-releases.)
pub fn latest_release_tag() -> Option<String> {
    let out = curl()
        .args(["-sIL", "-m", "10", "-o", "NUL", "-w", "%{url_effective}"])
        .arg(format!("https://github.com/{REPO}/releases/latest"))
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    tag_from_release_url(&String::from_utf8_lossy(&out.stdout))
}

/// `Some(tag)` when GitHub has a release newer than this build.
pub fn available_update() -> Option<String> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return None;
    }
    let tag = latest_release_tag()?;
    is_newer(env!("CARGO_PKG_VERSION"), &tag).then_some(tag)
}

/// Downloads release `tag` and swaps it in for the running executable. The
/// new exe is written next to the current one so the final rename never
/// crosses volumes.
pub fn download_and_install(tag: &str) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| format!("can't locate wgtui.exe: {e}"))?;
    let new = sibling(&current, ".new");
    let _ = std::fs::remove_file(&new);

    let status = curl()
        .args(["-fsSL", "-m", "300", "-o"])
        .arg(&new)
        .arg(format!(
            "https://github.com/{REPO}/releases/download/{tag}/wgtui.exe"
        ))
        .status()
        .map_err(|e| format!("couldn't run curl.exe: {e}"))?;
    let result = if status.success() {
        validate_download(&new).and_then(|()| {
            swap_executable(&current, &new).map_err(|e| format!("couldn't replace wgtui.exe: {e}"))
        })
    } else {
        Err(format!("download of {tag} failed (curl exit {status})"))
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_tags_and_plain_versions() {
        assert_eq!(parse_version("v0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("v10.0.12"), Some((10, 0, 12)));
        assert_eq!(parse_version(" v1.2.3\n"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_version_rejects_garbage() {
        for bad in ["", "v", "1.2", "1.2.3.4", "a.b.c", "latest", "v1.2.x"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn is_newer_compares_numerically_not_lexically() {
        assert!(is_newer("0.2.1", "v0.2.2"));
        assert!(is_newer("0.2.1", "v0.3.0"));
        assert!(is_newer("0.9.9", "v0.10.0"), "10 > 9, not '1' < '9'");
        assert!(is_newer("0.2.1", "v1.0.0"));
        assert!(!is_newer("0.2.1", "v0.2.1"), "same version");
        assert!(!is_newer("0.2.1", "v0.2.0"), "older release");
        assert!(!is_newer("0.2.1", "garbage"));
        assert!(!is_newer("garbage", "v9.9.9"));
    }

    #[test]
    fn tag_from_release_url_reads_the_redirect_target() {
        assert_eq!(
            tag_from_release_url(
                "https://github.com/raphaelantoniocampos/wgtui/releases/tag/v0.2.1"
            ),
            Some("v0.2.1".to_string())
        );
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases/tag/v1.0.0\r\n"),
            Some("v1.0.0".to_string())
        );
        // No release yet: /releases/latest doesn't redirect to a tag.
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases"),
            None
        );
        assert_eq!(tag_from_release_url(""), None);
    }

    fn write(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn validate_download_accepts_a_real_looking_exe() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"MZ".to_vec();
        bytes.resize(200 * 1024, 0);
        let p = write(dir.path(), "ok.exe", &bytes);
        assert!(validate_download(&p).is_ok());
    }

    #[test]
    fn validate_download_rejects_html_tiny_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();

        let mut html = b"<!DOCTYPE html><html>Not Found</html>".to_vec();
        html.resize(200 * 1024, b' ');
        assert!(validate_download(&write(dir.path(), "a.exe", &html)).is_err());

        assert!(validate_download(&write(dir.path(), "b.exe", b"MZ")).is_err());
        assert!(validate_download(&dir.path().join("missing.exe")).is_err());
    }

    #[test]
    fn swap_executable_moves_old_aside_and_installs_new() {
        let dir = tempfile::tempdir().unwrap();
        let current = write(dir.path(), "wgtui.exe", b"OLD");
        let new = write(dir.path(), "wgtui.exe.new", b"NEW");

        swap_executable(&current, &new).unwrap();

        assert_eq!(std::fs::read(&current).unwrap(), b"NEW");
        assert_eq!(
            std::fs::read(dir.path().join("wgtui.exe.old")).unwrap(),
            b"OLD"
        );
        assert!(!new.exists());
    }

    #[test]
    fn swap_executable_replaces_a_stale_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let current = write(dir.path(), "wgtui.exe", b"V2");
        write(dir.path(), "wgtui.exe.old", b"V1-from-last-update");
        let new = write(dir.path(), "wgtui.exe.new", b"V3");

        swap_executable(&current, &new).unwrap();

        assert_eq!(std::fs::read(&current).unwrap(), b"V3");
        assert_eq!(
            std::fs::read(dir.path().join("wgtui.exe.old")).unwrap(),
            b"V2"
        );
    }

    #[test]
    fn swap_executable_restores_the_old_exe_when_the_new_one_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let current = write(dir.path(), "wgtui.exe", b"OLD");
        let missing = dir.path().join("wgtui.exe.new");

        assert!(swap_executable(&current, &missing).is_err());

        assert_eq!(
            std::fs::read(&current).unwrap(),
            b"OLD",
            "a failed update must leave the working exe in place"
        );
    }
}
