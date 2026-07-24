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

/// Installs a package by its winget ID.
///
/// Returns `Ok(())` on success, `Err(message)` on failure.
pub fn install_package(id: &str) -> Result<String, String> {
    let output = Command::new("winget")
        .args([
            "install",
            "--exact",
            id,
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--scope",
            "machine",
        ])
        .output()
        .map_err(|e| format!("Failed to run winget install: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
    }
}

/// Runs `winget show <id>` to display package info.
pub fn show_package(id: &str) -> Result<String, String> {
    let output = Command::new("winget")
        .args(["show", id, "--accept-source-agreements"])
        .output()
        .map_err(|e| format!("Failed to run winget show: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
    }
}

/// Uninstalls a package by its winget ID.
pub fn uninstall_package(id: &str) -> Result<String, String> {
    let output = Command::new("winget")
        .args([
            "uninstall",
            "--exact",
            id,
            "--silent",
            "--accept-source-agreements",
        ])
        .output()
        .map_err(|e| format!("Failed to run winget uninstall: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
    }
}

/// Upgrades a package by its winget ID.
///
/// Returns `Ok(())` on success, `Err(message)` on failure.
pub fn upgrade_package(id: &str) -> Result<String, String> {
    let output = Command::new("winget")
        .args([
            "upgrade",
            "--exact",
            id,
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .output()
        .map_err(|e| format!("Failed to run winget upgrade: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
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

/// Runs a winget command and sends its stdout lines live through the sender.
/// The sender is dropped when the command finishes, signaling completion.
pub fn run_winget_stdout(args: &[&str], tx: mpsc::Sender<String>) -> Result<(), String> {
    let mut child = Command::new("winget")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn winget: {e}"))?;

    let stdout = child.stdout.take().ok_or("No stdout")?;
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
    let _ = child.wait();
    Ok(())
}

/// A package read from a winget export JSON file.
#[derive(Debug, Clone)]
pub struct JsonPackage {
    pub id: String,
}

// Serde types matching the winget export schema:
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

// Flat format: { "Packages": [{ "PackageIdentifier": "..." }] }
// No Sources wrapper, used by files like desired.json
#[derive(Deserialize)]
struct FlatPackageList {
    #[serde(rename = "Packages")]
    packages: Option<Vec<ExportPackage>>,
}

#[derive(Deserialize)]
struct ExportPackage {
    #[serde(rename = "PackageIdentifier")]
    package_identifier: String,
}

/// Scans `dir` for `*.json` files that match the winget export schema
/// and returns a merged deduplicated list of package identifiers.
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
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Quick validation: try both formats
        if serde_json::from_str::<ExportRoot>(&content).is_ok()
            || serde_json::from_str::<FlatPackageList>(&content).is_ok()
        {
            files.push(path);
        }
    }
    files
}

/// Loads packages from a single JSON file.
#[must_use]
pub fn load_packages_from_file(path: &Path) -> Vec<JsonPackage> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    // Try full winget export format first
    if let Ok(root) = serde_json::from_str::<ExportRoot>(&content) {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for source in root.sources {
            for pkg in source.packages {
                if seen.insert(pkg.package_identifier.clone()) {
                    result.push(JsonPackage { id: pkg.package_identifier });
                }
            }
        }
        return result;
    }

    // Fall back to flat format
    if let Ok(flat) = serde_json::from_str::<FlatPackageList>(&content) {
        if let Some(pkgs) = flat.packages {
            return pkgs.into_iter().map(|p| JsonPackage { id: p.package_identifier }).collect();
        }
    }

    vec![]
}

/// Scans `dir` for `*.json` files that match the winget export schema
/// and returns a merged deduplicated list of package identifiers.
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
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Try full winget export format first: { "Sources": [{ "Packages": [...] }] }
        if let Ok(root) = serde_json::from_str::<ExportRoot>(&content) {
            for source in root.sources {
                for pkg in source.packages {
                    if seen.insert(pkg.package_identifier.clone()) {
                        result.push(JsonPackage {
                            id: pkg.package_identifier.clone(),
                        });
                    }
                }
            }
        // Fall back to flat format: { "Packages": [{ "PackageIdentifier": "..." }] }
        } else if let Ok(flat) = serde_json::from_str::<FlatPackageList>(&content) {
            if let Some(pkgs) = flat.packages {
                for pkg in pkgs {
                    if seen.insert(pkg.package_identifier.clone()) {
                        result.push(JsonPackage {
                            id: pkg.package_identifier.clone(),
                        });
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_load_export_packages_valid() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
            "Sources": [
                {{
                    "Packages": [
                        {{ "PackageIdentifier": "7zip.7zip" }},
                        {{ "PackageIdentifier": "Google.Chrome" }}
                    ]
                }}
            ]
        }}"#
        )
        .unwrap();
        drop(f);

        let packages = load_export_packages(dir.path());
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].id, "7zip.7zip");
        assert_eq!(packages[1].id, "Google.Chrome");
    }

    #[test]
    fn test_load_export_packages_skips_invalid_json() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "not json").unwrap();
        drop(f);

        let packages = load_export_packages(dir.path());
        assert!(packages.is_empty());
    }

    #[test]
    fn test_load_export_packages_dedup() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
            "Sources": [
                {{
                    "Packages": [
                        {{ "PackageIdentifier": "7zip.7zip" }},
                        {{ "PackageIdentifier": "7zip.7zip" }}
                    ]
                }}
            ]
        }}"#
        )
        .unwrap();
        drop(f);

        let packages = load_export_packages(dir.path());
        assert_eq!(packages.len(), 1);
    }

    #[test]
    fn test_load_flat_format() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desired.json");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{
            "Packages": [
                {{ "PackageIdentifier": "Google.Chrome" }},
                {{ "PackageIdentifier": "Mozilla.Firefox" }}
            ]
        }}"#
        )
        .unwrap();
        drop(f);

        let packages = load_export_packages(dir.path());
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].id, "Google.Chrome");
        assert_eq!(packages[1].id, "Mozilla.Firefox");
    }
}
