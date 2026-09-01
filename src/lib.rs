#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use regex::Regex;
use serde::Deserialize;

/// A package returned by `winget search` or `winget list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WingetPackage {
    pub name: String,
    pub id: String,
    pub version: Option<String>,
    pub source: Option<String>,
}

/// Checks whether `winget` is available on `PATH` by running `winget --version`.
#[must_use]
pub fn check_winget() -> bool {
    Command::new("winget")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs `winget source update` to refresh the package source indexes.
///
/// Output is discarded. Returns whether it succeeded.
pub fn update_sources() -> bool {
    Command::new("winget")
        .args(["source", "update"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs `winget search <query>` and parses the tabular output into a list of packages.
///
/// Returns an empty vec on error.
#[must_use]
pub fn search_packages(query: &str) -> Vec<WingetPackage> {
    let output = Command::new("winget")
        .args(["search", query, "--accept-source-agreements"])
        .output();

    match output {
        Ok(output) => parse_winget_table(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => vec![],
    }
}

/// Runs `winget list` and parses the tabular output into a list of installed packages.
///
/// Returns an empty vec on error.
#[must_use]
pub fn list_installed() -> Vec<WingetPackage> {
    let output = Command::new("winget")
        .args(["list", "--accept-source-agreements"])
        .output();

    match output {
        Ok(output) => parse_winget_table(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => vec![],
    }
}

/// A package with an available upgrade, returned by `winget upgrade` (list mode).
#[derive(Debug, Clone)]
pub struct UpgradablePackage {
    pub name: String,
    pub id: String,
    pub installed_version: String,
    pub available_version: String,
    pub source: Option<String>,
}

/// Runs `winget upgrade` (list mode) to list packages with available upgrades.
#[must_use]
pub fn list_upgradable() -> Vec<UpgradablePackage> {
    let output = Command::new("winget")
        .args(["upgrade", "--accept-source-agreements"])
        .output();

    match output {
        Ok(output) => parse_upgrade_table(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => vec![],
    }
}

/// Runs `winget upgrade --all --include-unknown` to upgrade every package including unknown.
pub fn upgrade_all_packages() -> Result<String, String> {
    let output = Command::new("winget")
        .args([
            "upgrade",
            "--all",
            "--include-unknown",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .output()
        .map_err(|e| format!("Failed to run winget upgrade --all --include-unknown: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
    }
}

/// Parses the tabular output of `winget upgrade` (list mode).
///
/// Table format: Name, Id, Version, Available, Source
fn parse_upgrade_table(output: &str) -> Vec<UpgradablePackage> {
    let re_spaces = Regex::new(r"\s{2,}").expect("regex: two or more whitespace");
    let lines: Vec<&str> = output.lines().collect();

    let header_idx = lines.iter().position(|line| {
        let lower = line.to_lowercase();
        (lower.contains("name") || lower.contains("nome")) && lower.contains("id")
    });

    let Some(header_idx) = header_idx else {
        return vec![];
    };

    let data_lines = &lines[header_idx + 1..];
    let mut packages = Vec::new();

    for line in data_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains("---") {
            continue;
        }

        let parts: Vec<&str> = re_spaces.splitn(trimmed, 5).collect();
        if parts.len() >= 4 {
            packages.push(UpgradablePackage {
                name: parts[0].trim().to_string(),
                id: parts[1].trim().to_string(),
                installed_version: parts[2].trim().to_string(),
                available_version: parts[3].trim().to_string(),
                source: parts
                    .get(4)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            });
        }
    }

    packages
}

/// Parses the tabular output of `winget search` / `winget list` into structured records.
///
/// The table format is:
/// ```text
/// Name                  ID                    Version           Source
/// ---------------------------------------------------------------
/// Google Chrome         Google.Chrome         134.0.6998.165    winget
/// ```
///
/// Columns are separated by 2+ spaces. The header row is detected by containing "Name" and "Id".
fn parse_winget_table(output: &str) -> Vec<WingetPackage> {
    let re_spaces = Regex::new(r"\s{2,}").expect("regex: two or more whitespace");
    let lines: Vec<&str> = output.lines().collect();

    // Find the header row (contains "Name" or "Nome" and "ID" or "Id")
    let header_idx = lines.iter().position(|line| {
        let lower = line.to_lowercase();
        (lower.contains("name") || lower.contains("nome")) && lower.contains("id")
    });

    let Some(header_idx) = header_idx else {
        return vec![];
    };

    // Parse data rows after header
    let data_lines = &lines[header_idx + 1..];
    let mut packages = Vec::new();

    for line in data_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains("---") {
            continue;
        }

        // Split by 2+ spaces, max 4 parts (Name, ID, Version, Source)
        let parts: Vec<&str> = re_spaces.splitn(trimmed, 4).collect();
        if parts.len() >= 2 {
            packages.push(WingetPackage {
                name: parts[0].trim().to_string(),
                id: parts[1].trim().to_string(),
                version: parts
                    .get(2)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                source: parts
                    .get(3)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            });
        }
    }

    packages
}

/// Runs a command and sends its output lines live through the sender.
///
/// Both stdout and stderr are forwarded (stderr is read on its own thread so a
/// full stderr pipe can't deadlock stdout). The sender is dropped when the
/// command finishes, signaling completion.
pub fn run_command_stdout(
    cmd: &str,
    args: &[&str],
    tx: mpsc::Sender<String>,
) -> Result<(), String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn {cmd}: {e}"))?;

    let stdout = child.stdout.take().ok_or("No stdout")?;
    let stderr = child.stderr.take().ok_or("No stderr")?;

    let stderr_tx = tx.clone();
    let stderr_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if stderr_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        match line {
            Ok(l) => {
                if tx.send(l).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = stderr_handle.join();
    let _ = child.wait();
    Ok(())
}

/// Runs a winget command and sends its stdout lines live through the sender.
pub fn run_winget_stdout(args: &[&str], tx: mpsc::Sender<String>) -> Result<(), String> {
    run_command_stdout("winget", args, tx)
}

/// Whether the manifest entry `id` / `name` matches something in `installed`.
///
/// `winget list` output routinely differs in case from `winget search`, and
/// truncates long names/IDs with an ellipsis, so the match is case-insensitive
/// on both fields with an ellipsis-prefix fallback.
#[must_use]
pub fn is_installed(id: &str, name: &str, installed: &[WingetPackage]) -> bool {
    let id_l = id.trim().to_lowercase();
    let name_l = name.trim().to_lowercase();

    let prefix_match = |listed: &str, full: &str| {
        let stem = listed.trim_end_matches(['…', '.']).trim();
        stem.len() >= 4 && full.starts_with(stem)
    };

    installed.iter().any(|p| {
        let p_id = p.id.to_lowercase();
        let p_name = p.name.to_lowercase();
        (!id_l.is_empty() && (p_id == id_l || prefix_match(&p_id, &id_l)))
            || (!name_l.is_empty() && (p_name == name_l || prefix_match(&p_name, &name_l)))
    })
}

/// A package or script read from a manifest file.
#[derive(Debug, Clone)]
pub struct JsonPackage {
    pub id: String,
    pub name: String,
    /// For scripts: the argv to run. `None` for winget packages.
    pub command: Option<Vec<String>>,
    pub is_script: bool,
    /// Extra args appended to `winget install` (e.g. `["-a", "x86"]`).
    pub args: Vec<String>,
    /// `--scope` value; defaults to `machine` when `None`.
    pub scope: Option<String>,
    /// `--locale` value; omitted when `None`.
    pub locale: Option<String>,
}

impl JsonPackage {
    /// The full `winget` argument list to install this package.
    ///
    /// Only meaningful for non-script entries.
    #[must_use]
    pub fn install_args(&self) -> Vec<String> {
        let mut a: Vec<String> = [
            "install",
            "--exact",
            &self.id,
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        a.push("--scope".to_string());
        a.push(self.scope.clone().unwrap_or_else(|| "machine".to_string()));

        if let Some(locale) = &self.locale {
            a.push("--locale".to_string());
            a.push(locale.clone());
        }

        a.extend(self.args.iter().cloned());
        a
    }
}

// -------- canonical wgtui manifest schema --------
// { "packages": [{ "id", "name?", "args?", "scope?", "locale?" }],
//   "scripts":  [{ "name", "command": [..] }] }

#[derive(Deserialize)]
struct Manifest {
    packages: Option<Vec<ManifestPackage>>,
    scripts: Option<Vec<ManifestScript>>,
}

#[derive(Deserialize)]
struct ManifestPackage {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Deserialize)]
struct ManifestScript {
    name: String,
    command: Vec<String>,
}

// -------- winget export schema (import interop only) --------
// https://aka.ms/winget-packages.schema.2.0.json

#[derive(Deserialize)]
struct ExportRoot {
    #[serde(rename = "Sources")]
    sources: Vec<ExportSource>,
}

#[derive(Deserialize)]
struct ExportSource {
    #[serde(rename = "Packages")]
    packages: Vec<ExportPackage>,
}

#[derive(Deserialize)]
struct ExportPackage {
    #[serde(rename = "PackageIdentifier")]
    package_identifier: String,
    #[serde(rename = "PackageName", default)]
    package_name: Option<String>,
}

/// True if `content` parses as JSON and carries a key of a recognized manifest
/// schema (`packages`/`scripts` for the wgtui format, `Sources` for winget export).
fn looks_like_manifest(content: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => {
            v.get("packages").is_some() || v.get("scripts").is_some() || v.get("Sources").is_some()
        }
        Err(_) => false,
    }
}

/// Scans `dir` for `*.json` files that look like a wgtui manifest or a winget
/// export, returning their paths.
/// Never panics — returns an empty vec on any error.
#[must_use]
pub fn find_package_json_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if fs::read_to_string(&path)
            .map(|c| looks_like_manifest(&c))
            .unwrap_or(false)
        {
            files.push(path);
        }
    }
    files
}

/// Loads packages and scripts from a single manifest file.
///
/// Accepts the canonical wgtui schema first; falls back to importing a
/// `winget export` file. Returns an empty vec for anything else.
#[must_use]
pub fn load_packages_from_file(path: &Path) -> Vec<JsonPackage> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    // Canonical wgtui manifest (at least one of `packages` / `scripts` present).
    if let Ok(manifest) = serde_json::from_str::<Manifest>(&content)
        && (manifest.packages.is_some() || manifest.scripts.is_some())
    {
        let mut result = Vec::new();
        for pkg in manifest.packages.unwrap_or_default() {
            let name = pkg.name.unwrap_or_else(|| pkg.id.clone());
            result.push(JsonPackage {
                id: pkg.id,
                name,
                command: None,
                is_script: false,
                args: pkg.args,
                scope: pkg.scope,
                locale: pkg.locale,
            });
        }
        for script in manifest.scripts.unwrap_or_default() {
            result.push(JsonPackage {
                id: script.name.clone(),
                name: script.name,
                command: Some(script.command),
                is_script: true,
                args: Vec::new(),
                scope: None,
                locale: None,
            });
        }
        return result;
    }

    // winget export import.
    if let Ok(root) = serde_json::from_str::<ExportRoot>(&content) {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for source in root.sources {
            for pkg in source.packages {
                if seen.insert(pkg.package_identifier.clone()) {
                    let name = pkg
                        .package_name
                        .unwrap_or_else(|| pkg.package_identifier.clone());
                    result.push(JsonPackage {
                        id: pkg.package_identifier,
                        name,
                        command: None,
                        is_script: false,
                        args: Vec::new(),
                        scope: None,
                        locale: None,
                    });
                }
            }
        }
        return result;
    }

    vec![]
}

/// Scans `dir` for `*.json` files that match the schemas
/// and returns a merged deduplicated list of packages and scripts.
/// Never panics — returns an empty vec on any error.
#[must_use]
pub fn load_export_packages(dir: &Path) -> Vec<JsonPackage> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let pkgs = load_packages_from_file(&path);
        for pkg in pkgs {
            if seen.insert(pkg.id.clone()) {
                result.push(pkg);
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn test_run_command_stdout_captures_stdout() {
        let (tx, rx) = mpsc::channel();
        run_command_stdout("cmd", &["/C", "echo", "hello_out"], tx).unwrap();
        let lines: Vec<String> = rx.iter().collect();
        assert!(
            lines.iter().any(|l| l.contains("hello_out")),
            "stdout line missing, got: {lines:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_command_stdout_captures_stderr() {
        let (tx, rx) = mpsc::channel();
        // `echo` piped to stderr via cmd redirection.
        run_command_stdout("cmd", &["/C", "echo hello_err 1>&2"], tx).unwrap();
        let lines: Vec<String> = rx.iter().collect();
        assert!(
            lines.iter().any(|l| l.contains("hello_err")),
            "stderr line missing, got: {lines:?}"
        );
    }

    #[test]
    fn test_parse_winget_table() {
        let sample = "\
Name                  ID                    Version           Source
-------------------------------------------------------------------
Google Chrome         Google.Chrome         134.0.6998.165    winget
7zip.7zip             7zip.7zip             24.09              winget
";

        let packages = parse_winget_table(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Google Chrome");
        assert_eq!(packages[0].id, "Google.Chrome");
        assert_eq!(packages[0].version.as_deref(), Some("134.0.6998.165"));
        assert_eq!(packages[0].source.as_deref(), Some("winget"));
        assert_eq!(packages[1].name, "7zip.7zip");
        assert_eq!(packages[1].id, "7zip.7zip");
    }

    #[test]
    fn test_parse_empty_table() {
        let packages = parse_winget_table("No installed package found");
        assert!(packages.is_empty());
    }

    #[test]
    fn test_parse_no_header() {
        let packages = parse_winget_table("");
        assert!(packages.is_empty());
    }

    fn write_tmp(name: &str, content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        drop(f);
        (dir, path)
    }

    fn wp(name: &str, id: &str) -> WingetPackage {
        WingetPackage {
            name: name.to_string(),
            id: id.to_string(),
            version: None,
            source: None,
        }
    }

    #[test]
    fn test_is_installed_matches_case_and_truncation() {
        let installed = vec![
            wp("Google Chrome", "Google.Chrome"),
            wp("Microsoft Visual Studio Code", "Microsoft.VisualStudioCo…"),
        ];
        // exact id, different case
        assert!(is_installed("google.chrome", "whatever", &installed));
        // name match, different case
        assert!(is_installed("Some.Other.Id", "GOOGLE CHROME", &installed));
        // winget-truncated id in the installed list
        assert!(is_installed(
            "Microsoft.VisualStudioCode",
            "VS Code",
            &installed
        ));
        // no match
        assert!(!is_installed("Mozilla.Firefox", "Firefox", &installed));
        // empty list
        assert!(!is_installed("Google.Chrome", "Google Chrome", &[]));
    }

    #[test]
    fn test_load_canonical_manifest() {
        let (_d, path) = write_tmp(
            "packages.json",
            r#"{
                "packages": [
                    { "id": "Google.Chrome", "name": "Google Chrome" },
                    { "id": "CPUID.CPU-Z" }
                ],
                "scripts": [
                    { "name": "Ativar Windows",
                      "command": ["powershell", "-Command", "irm https://get.activated.win | iex"] }
                ]
            }"#,
        );

        let packages = load_packages_from_file(&path);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "Google Chrome");
        assert!(!packages[0].is_script);
        assert_eq!(packages[1].name, "CPUID.CPU-Z"); // name falls back to id
        assert!(packages[2].is_script);
        assert_eq!(packages[2].command.as_ref().unwrap()[0], "powershell");
    }

    #[test]
    fn test_manifest_install_args_scope_locale_and_extra() {
        let (_d, path) = write_tmp(
            "packages.json",
            r#"{
                "packages": [
                    { "id": "Oracle.JavaRuntimeEnvironment", "name": "Java x86",
                      "args": ["-a", "x86", "--force"], "scope": "user", "locale": "pt-BR" }
                ]
            }"#,
        );

        let pkg = &load_packages_from_file(&path)[0];
        let args = pkg.install_args();
        let pair = |a: &str, b: &str| args.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(args.starts_with(&["install".to_string(), "--exact".to_string()]));
        assert!(pair("--scope", "user"));
        assert!(pair("--locale", "pt-BR"));
        assert!(pair("-a", "x86"));
        assert!(args.contains(&"--force".to_string()));
    }

    #[test]
    fn test_install_args_default_scope_is_machine() {
        let pkg = JsonPackage {
            id: "Google.Chrome".to_string(),
            name: "Google Chrome".to_string(),
            command: None,
            is_script: false,
            args: Vec::new(),
            scope: None,
            locale: None,
        };
        let args = pkg.install_args();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--scope" && w[1] == "machine")
        );
        assert!(!args.iter().any(|a| a == "--locale"));
    }

    #[test]
    fn test_load_winget_export_import_with_dedup() {
        let (_d, path) = write_tmp(
            "exported.json",
            r#"{
                "Sources": [
                    { "Packages": [
                        { "PackageIdentifier": "7zip.7zip" },
                        { "PackageIdentifier": "Google.Chrome", "PackageName": "Chrome" },
                        { "PackageIdentifier": "7zip.7zip" }
                    ] }
                ]
            }"#,
        );

        let packages = load_packages_from_file(&path);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].id, "7zip.7zip");
        assert_eq!(packages[0].name, "7zip.7zip"); // name falls back to id
        assert_eq!(packages[1].name, "Chrome");
        assert!(packages.iter().all(|p| p.args.is_empty()));
    }

    #[test]
    fn test_load_rejects_plain_json_and_missing_file() {
        let (_d, path) = write_tmp("random.json", r#"{ "hello": "world" }"#);
        assert!(load_packages_from_file(&path).is_empty());
        assert!(load_packages_from_file(std::path::Path::new("does-not-exist.json")).is_empty());
    }

    #[test]
    fn test_find_package_json_files_recognizes_schemas() {
        let (dir, _p) = write_tmp("manifest.json", r#"{ "packages": [{ "id": "A.B" }] }"#);
        write_tmp_in(dir.path(), "export.json", r#"{ "Sources": [] }"#);
        write_tmp_in(dir.path(), "notes.json", r#"{ "unrelated": true }"#);
        write_tmp_in(dir.path(), "readme.txt", "not json at all");

        let mut names: Vec<String> = find_package_json_files(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["export.json", "manifest.json"]);
    }

    fn write_tmp_in(dir: &std::path::Path, name: &str, content: &str) {
        use std::io::Write;
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_load_export_packages_merges_directory() {
        let (dir, _p) = write_tmp("a.json", r#"{ "packages": [{ "id": "A.One" }] }"#);
        write_tmp_in(
            dir.path(),
            "b.json",
            r#"{ "packages": [{ "id": "B.Two" }, { "id": "A.One" }] }"#,
        );

        let packages = load_export_packages(dir.path());
        let ids: std::collections::HashSet<&str> = packages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(packages.len(), 2); // A.One deduped across files
        assert!(ids.contains("A.One") && ids.contains("B.Two"));
    }
}
