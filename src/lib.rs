#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

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

/// Runs `winget upgrade --all --include-unknown` to upgrade every package
/// including unknown. See [`run_command_stdout`] for `pid_slot`.
pub fn upgrade_all_packages(pid_slot: Option<&PidSlot>) -> Result<String, String> {
    let child = Command::new("winget")
        .args([
            "upgrade",
            "--all",
            "--include-unknown",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run winget upgrade --all --include-unknown: {e}"))?;

    if let Some(slot) = pid_slot {
        *slot.lock().unwrap() = Some(child.id());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait on winget upgrade --all --include-unknown: {e}"))?;

    if let Some(slot) = pid_slot {
        *slot.lock().unwrap() = None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        Err(msg)
    }
}

/// Finds the *character* index (not byte index) of `needle` in `haystack`,
/// ASCII-case-insensitively (winget's table headers are ASCII apart from a
/// handful of accented Portuguese letters — e.g. "Versão", "Disponível" —
/// compared literally, since winget always emits them in the same case).
///
/// Character indices, not byte offsets, are what make column positions
/// portable between the header and a data line: winget aligns columns by
/// display width, and a header with e.g. "Versão" (one 2-byte "ã") has a
/// different byte-length-to-character-count ratio than a data line of plain
/// ASCII — or than another line whose *package name* happens to contain
/// accented/non-ASCII characters. Byte offsets computed from one line and
/// applied to another silently drift; character offsets don't.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    let hay: Vec<char> = haystack.chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&start| {
        hay[start..start + needle.len()]
            .iter()
            .zip(&needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Locates the start column (character index) of each header label in
/// `header`, in order. Each entry in `labels` lists the accepted spellings
/// for that column (e.g. English/Portuguese); the first one found is used.
/// `None` if any column's header can't be located.
fn locate_columns(header: &str, labels: &[&[&str]]) -> Option<Vec<usize>> {
    labels
        .iter()
        .map(|candidates| candidates.iter().find_map(|c| find_ci(header, c)))
        .collect()
}

/// Splits `line` into columns at the given fixed *character* offsets (from
/// `locate_columns`), trimming each.
///
/// winget's table columns are fixed-width and left-aligned under the header,
/// so slicing by position — not by runs of whitespace — is required: a
/// genuinely empty cell (common in the Id column for manually-installed
/// apps winget can't match to a source) must not shift every later column
/// left by one. That shift is exactly what used to put a package's Version
/// string where its Id belonged.
fn slice_columns<'a>(line: &'a str, offsets: &[usize]) -> Vec<&'a str> {
    // This line's own char-index -> byte-offset table (see `find_ci`'s doc
    // for why offsets can't just be reused as byte indices directly).
    let mut char_byte: Vec<usize> = line.char_indices().map(|(b, _)| b).collect();
    char_byte.push(line.len());
    let to_byte = |ch: usize| char_byte.get(ch).copied().unwrap_or(line.len());

    offsets
        .iter()
        .enumerate()
        .map(|(i, &start_ch)| {
            let start = to_byte(start_ch);
            let end = offsets
                .get(i + 1)
                .map(|&e| to_byte(e))
                .unwrap_or(line.len())
                .max(start);
            line[start..end].trim()
        })
        .collect()
}

/// Finds the header row: the first line containing both a name-ish and an
/// id-ish column label.
fn find_header(lines: &[&str]) -> Option<usize> {
    lines.iter().position(|line| {
        let lower = line.to_lowercase();
        (lower.contains("name") || lower.contains("nome")) && lower.contains("id")
    })
}

/// Best-effort check that `line` has real column structure — i.e. it's a
/// genuine data row, not a stray summary/warning line winget sometimes
/// prints right after the table with no blank-line separator (e.g. "4
/// atualizações disponíveis.", a pinned-package notice), which this table's
/// row loop would otherwise slice into a bogus package.
///
/// Real rows are fixed-width columns, so there's essentially always at least
/// one run of 2+ consecutive spaces somewhere on the line — even a row whose
/// own content happens to fill some column tightly (a single space before
/// the next column) still has slack in at least one other, typically before
/// the short, consistent Source column. Ordinary prose never double-spaces
/// between words, so this holds even when a summary sentence coincidentally
/// has a single space lined up with a column boundary.
fn looks_like_row(line: &str) -> bool {
    line.as_bytes().windows(2).any(|w| w == b"  ")
}

/// Parses the tabular output of `winget upgrade` (list mode).
///
/// Table format: Name, Id, Version, Available, Source
fn parse_upgrade_table(output: &str) -> Vec<UpgradablePackage> {
    let lines: Vec<&str> = output.lines().collect();
    let Some(header_idx) = find_header(&lines) else {
        return vec![];
    };
    let header = lines[header_idx];
    let Some(offsets) = locate_columns(
        header,
        &[
            &["Name", "Nome"],
            &["Id"],
            &["Version", "Versão"],
            &["Available", "Disponível"],
            &["Source", "Origem"],
        ],
    ) else {
        return vec![];
    };

    let mut packages = Vec::new();
    for line in &lines[header_idx + 1..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("---") || !looks_like_row(line) {
            continue;
        }
        let cols = slice_columns(line, &offsets);
        if cols[0].is_empty() {
            continue; // no name at all — not a usable row
        }
        packages.push(UpgradablePackage {
            name: cols[0].to_string(),
            id: cols[1].to_string(),
            installed_version: cols[2].to_string(),
            available_version: cols[3].to_string(),
            source: Some(cols[4].to_string()).filter(|s| !s.is_empty()),
        });
    }
    packages
}

/// Parses the tabular output of `winget search` / `winget list` into structured records.
///
/// The table format is fixed-width columns under a header, e.g.:
/// ```text
/// Name                  ID                    Version           Source
/// ---------------------------------------------------------------
/// Google Chrome         Google.Chrome         134.0.6998.165    winget
/// ```
/// Columns are located by the header labels' positions (not by runs of
/// whitespace), so a row with a blank Id (or Version, or Source) keeps every
/// other column in its correct place instead of shifting left.
///
/// Depending on the winget version and subcommand, an extra column can
/// appear between Version and Source that this table has no field for — an
/// available update's version (`list`) or why a search result matched
/// (`search`, "Moniker: ..."/"Tag: ..."). If present, it's located too,
/// purely to correctly bound the Version column, and its own text discarded
/// — otherwise it would run on into `version` (e.g. `"4.90.0  4.91.0"`).
fn parse_winget_table(output: &str) -> Vec<WingetPackage> {
    let lines: Vec<&str> = output.lines().collect();
    let Some(header_idx) = find_header(&lines) else {
        return vec![];
    };
    let header = lines[header_idx];
    let name_off = find_ci(header, "Name").or_else(|| find_ci(header, "Nome"));
    let id_off = find_ci(header, "ID").or_else(|| find_ci(header, "Id"));
    let version_off = find_ci(header, "Version").or_else(|| find_ci(header, "Versão"));
    let source_off = find_ci(header, "Source").or_else(|| find_ci(header, "Origem"));
    let (Some(name_off), Some(id_off), Some(version_off), Some(source_off)) =
        (name_off, id_off, version_off, source_off)
    else {
        return vec![];
    };
    let version_end = ["Available", "Disponível", "Match", "Correspondência"]
        .iter()
        .find_map(|l| find_ci(header, l))
        .filter(|&o| o > version_off && o < source_off)
        .unwrap_or(source_off);
    let offsets = [name_off, id_off, version_off, version_end, source_off];

    let mut packages = Vec::new();
    for line in &lines[header_idx + 1..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("---") || !looks_like_row(line) {
            continue;
        }
        let cols = slice_columns(line, &offsets);
        if cols[0].is_empty() {
            continue; // no name at all — not a usable row
        }
        packages.push(WingetPackage {
            name: cols[0].to_string(),
            id: cols[1].to_string(),
            version: Some(cols[2].to_string()).filter(|s| !s.is_empty()),
            source: Some(cols[4].to_string()).filter(|s| !s.is_empty()),
        });
    }
    packages
}

/// Shared slot the PID of a currently-running child process is published
/// into, so another thread can look it up and kill it (see
/// [`kill_process_tree`]). `None` when nothing is running.
pub type PidSlot = Arc<Mutex<Option<u32>>>;

/// Runs a command and sends its output lines live through the sender.
///
/// Both stdout and stderr are forwarded (stderr is read on its own thread so a
/// full stderr pipe can't deadlock stdout). The sender is dropped when the
/// command finishes, signaling completion. If `pid_slot` is given, the
/// child's PID is published into it right after spawning and cleared again
/// once the command finishes, so a caller elsewhere can cancel it.
pub fn run_command_stdout(
    cmd: &str,
    args: &[&str],
    tx: mpsc::Sender<String>,
    pid_slot: Option<&PidSlot>,
) -> Result<(), String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn {cmd}: {e}"))?;

    if let Some(slot) = pid_slot {
        *slot.lock().unwrap() = Some(child.id());
    }

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
    if let Some(slot) = pid_slot {
        *slot.lock().unwrap() = None;
    }
    Ok(())
}

/// Runs a winget command and sends its stdout lines live through the sender.
/// See [`run_command_stdout`] for `pid_slot`.
pub fn run_winget_stdout(
    args: &[&str],
    tx: mpsc::Sender<String>,
    pid_slot: Option<&PidSlot>,
) -> Result<(), String> {
    run_command_stdout("winget", args, tx, pid_slot)
}

/// Kills the process tree rooted at `pid`.
///
/// Best-effort: the target may already have exited by the time this runs, in
/// which case `taskkill` fails and this returns `false` — harmless, not an
/// error condition worth surfacing. Windows-only, like the rest of this crate.
pub fn kill_process_tree(pid: u32) -> bool {
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A pending Windows Update item, as reported by `Get-WindowsUpdate`
/// (PSWindowsUpdate module).
///
/// Every field but `title` is optional: the exact JSON shape PSWindowsUpdate
/// produces hasn't been observed against a real machine with pending updates
/// (unlike winget's table output, which was validated against real captures
/// from this machine) — parsing must tolerate an unexpected/missing field
/// rather than fail outright.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WindowsUpdateItem {
    #[serde(rename = "KB", default)]
    pub kb: Option<String>,
    #[serde(rename = "Title", default)]
    pub title: String,
    #[serde(rename = "Size", default)]
    pub size: Option<String>,
}

/// Parses the JSON a Windows Update check writes out (see
/// [`windows_update_check_script`]). Never panics: malformed input is an
/// `Err`, not a crash.
pub fn parse_windows_update_json(raw: &str) -> Result<Vec<WindowsUpdateItem>, String> {
    serde_json::from_str(raw).map_err(|e| format!("Failed to parse Windows Update JSON: {e}"))
}

/// Where a Windows Update check writes its JSON result, for
/// [`read_windows_update_result`] to read back after the process exits.
#[must_use]
pub fn windows_update_json_path() -> std::path::PathBuf {
    std::env::temp_dir().join("wgtui-windows-update.json")
}

/// Builds the PowerShell one-liner that lists pending Windows updates as
/// JSON at `json_path`.
///
/// Ensures the PSWindowsUpdate module is present (installing it and its
/// NuGet provider at `-Scope CurrentUser`, so this never requires
/// Administrator — mirrors `bootstrap.rs`'s `BOOTSTRAP_PS`), then writes
/// `Get-WindowsUpdate`'s result to `json_path` rather than relying on
/// captured stdout: PSWindowsUpdate is a chatty module (progress/verbose
/// output), so decoupling the structured result from whatever incidental
/// text it prints along the way avoids an entire class of "stdout wasn't
/// purely the data I expected" bug.
///
/// `Remove-Item` on `json_path` runs first, before anything that could fail,
/// so "the file is missing after the process exits" always means "the check
/// didn't complete" — never a stale result from a previous run being reread
/// by mistake.
///
/// Two `ConvertTo-Json` pitfalls, both guarded against: `@(...)` around the
/// `Get-WindowsUpdate` pipeline forces array context so a single pending
/// update still serializes as a one-element JSON array instead of collapsing
/// to a bare object; and `-InputObject $updates` (not `$updates |
/// ConvertTo-Json`) passes the array as one value instead of piping it
/// element-by-element — piped, an *empty* array sends zero objects through
/// the pipeline, so `ConvertTo-Json` receives no input at all and emits
/// nothing, leaving a file that fails to parse as JSON (found by testing
/// against a real machine with zero pending updates). Both together mean 0,
/// 1, and N pending updates all serialize as a proper JSON array.
#[must_use]
pub fn windows_update_check_script(json_path: &Path) -> String {
    let path = json_path.display();
    format!(
        "Remove-Item -Path '{path}' -ErrorAction SilentlyContinue; \
         if (-not (Get-Module -ListAvailable | Where-Object {{ $_.Name -eq 'PSWindowsUpdate' }})) {{ \
         Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 -Force -Scope CurrentUser; \
         Install-Module -Name PSWindowsUpdate -Force -Scope CurrentUser }}; \
         Import-Module PSWindowsUpdate; \
         $updates = @(Get-WindowsUpdate | Select-Object KB, Title, Size); \
         ConvertTo-Json -InputObject $updates -Depth 3 | Out-File -FilePath '{path}' -Encoding utf8"
    )
}

/// PowerShell one-liner that installs every pending Windows update.
///
/// Ensures PSWindowsUpdate is present (same `-Scope CurrentUser` guard as
/// [`windows_update_check_script`] — this line alone never needs
/// Administrator; the actual install, like Windows Update itself, does), then
/// runs `-AcceptAll -Install -IgnoreReboot`: accepts every pending update and
/// installs it, but never reboots the machine on its own.
///
/// Deliberately does *not* mirror the caller's original
/// `Set-ExecutionPolicy -Scope LocalMachine` bookending: that needs
/// Administrator just to run, and has a real bug — if `Get-WindowsUpdate`
/// throws partway through, the final line restoring the policy never runs,
/// permanently altering the machine's execution policy. The process-scoped
/// `-ExecutionPolicy Bypass` flag passed to `powershell.exe` (see
/// `run_command_stdout` call sites) achieves the same thing without needing
/// admin or mutating any persistent state — already the established pattern
/// in this codebase (`bootstrap.rs`'s `BOOTSTRAP_PS`).
pub const WINDOWS_UPDATE_INSTALL_PS: &str = "\
    if (-not (Get-Module -ListAvailable | Where-Object { $_.Name -eq 'PSWindowsUpdate' })) { \
    Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 -Force -Scope CurrentUser; \
    Install-Module -Name PSWindowsUpdate -Force -Scope CurrentUser }; \
    Import-Module PSWindowsUpdate; \
    Get-WindowsUpdate -AcceptAll -Install -IgnoreReboot";

/// Reads and parses the JSON a Windows Update check wrote to `json_path`.
///
/// A missing file (the check never completed, or hasn't run yet) is reported
/// as its own distinct error rather than silently treated as "zero updates".
pub fn read_windows_update_result(json_path: &Path) -> Result<Vec<WindowsUpdateItem>, String> {
    let raw = fs::read_to_string(json_path)
        .map_err(|e| format!("Windows Update check produced no result: {e}"))?;
    parse_windows_update_json(&raw)
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
    /// `--scope` value; omitted (let winget pick) when `None`. Many packages
    /// only ship a user-scope installer, so forcing `machine` by default made
    /// otherwise-fine installs fail outright.
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

        if let Some(scope) = &self.scope {
            a.push("--scope".to_string());
            a.push(scope.clone());
        }

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
        run_command_stdout("cmd", &["/C", "echo", "hello_out"], tx, None).unwrap();
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
        run_command_stdout("cmd", &["/C", "echo hello_err 1>&2"], tx, None).unwrap();
        let lines: Vec<String> = rx.iter().collect();
        assert!(
            lines.iter().any(|l| l.contains("hello_err")),
            "stderr line missing, got: {lines:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_run_command_stdout_publishes_and_clears_pid() {
        let (tx, rx) = mpsc::channel();
        let slot: PidSlot = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();
        run_command_stdout("cmd", &["/C", "echo", "hi"], tx, Some(&slot2)).unwrap();
        let _ = rx.iter().collect::<Vec<_>>();
        // The command already finished, so the slot must be cleared again.
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_kill_process_tree_stops_a_running_child() {
        use std::time::{Duration, Instant};

        let (tx, rx) = mpsc::channel();
        let slot: PidSlot = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();
        // `ping`, unlike `timeout`, doesn't refuse to run with redirected
        // stdin, so it reliably blocks for the full duration under `cargo test`.
        let handle = std::thread::spawn(move || {
            run_command_stdout("ping", &["-n", "11", "127.0.0.1"], tx, Some(&slot2))
        });

        let pid = loop {
            if let Some(p) = *slot.lock().unwrap() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        let start = Instant::now();
        assert!(kill_process_tree(pid));
        let _ = rx.iter().collect::<Vec<_>>();
        handle.join().unwrap().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "child should die well before its 10s timeout"
        );
    }

    #[test]
    fn test_kill_process_tree_on_a_dead_pid_is_a_harmless_no_op() {
        // A PID that (almost certainly) doesn't exist; taskkill should fail
        // gracefully rather than panic.
        let _ = kill_process_tree(u32::MAX);
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
    fn test_parse_winget_table_blank_id_does_not_leak_into_version() {
        // Manually-installed apps winget can't match to a source often show
        // a blank Id column. Reported bug: the old whitespace-splitting
        // parser then shifted the Version string into the Id field, so
        // upgrade/remove/show sent that version string as `--exact <id>`.
        let sample = "\
Name                  ID                    Version           Source
-------------------------------------------------------------------
Google Chrome         Google.Chrome         134.0.6998.165    winget
Some Weird App                              9.9.9
";
        let packages = parse_winget_table(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "Some Weird App");
        assert_eq!(packages[1].id, "", "no Id column, not the version string");
        assert_eq!(packages[1].version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn test_parse_winget_table_real_list_output_with_available_column() {
        // Captured live from `winget list --accept-source-agreements`
        // (v1.29.290, pt-BR): `list` adds a 5th "Disponível" (Available)
        // column whenever *any* row has a pending update — WingetPackage has
        // no field for it, but it must not run on into `version` for rows
        // that populate it.
        let sample = "Nome                                                                                                  ID                                                                                              Versão                        Disponível Origem\n\
------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------\n\
Docker Desktop                                                                                        Docker.DockerDesktop                                                                            4.90.0                        4.91.0     winget\n\
bat                                                                                                   sharkdp.bat                                                                                     0.26.1                                   winget\n";

        let packages = parse_winget_table(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "Docker Desktop");
        assert_eq!(packages[0].id, "Docker.DockerDesktop");
        assert_eq!(
            packages[0].version.as_deref(),
            Some("4.90.0"),
            "the 4.91.0 available-version text must not leak in"
        );
        assert_eq!(packages[1].name, "bat");
        assert_eq!(packages[1].id, "sharkdp.bat");
        assert_eq!(packages[1].version.as_deref(), Some("0.26.1"));
        assert_eq!(packages[1].source.as_deref(), Some("winget"));
    }

    #[test]
    fn test_parse_winget_table_real_search_output_with_match_column() {
        // Captured live from `winget search 7zip --accept-source-agreements`
        // (pt-BR): `search` adds a "Correspondência" (Match reason) column
        // that must likewise not leak into `version`.
        let sample = "\
Nome                               ID                        Versão          Correspondência Origem
---------------------------------------------------------------------------------------------------
7-Zip                              7zip.7zip                 26.03           Moniker: 7zip   winget
7zr                                7zip.7zr                  26.03                           winget
";
        let packages = parse_winget_table(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "7-Zip");
        assert_eq!(packages[0].id, "7zip.7zip");
        assert_eq!(packages[0].version.as_deref(), Some("26.03"));
        assert_eq!(packages[0].source.as_deref(), Some("winget"));
        assert_eq!(packages[1].id, "7zip.7zr");
        assert_eq!(packages[1].version.as_deref(), Some("26.03"));
    }

    #[test]
    fn test_parse_upgrade_table_real_output_skips_summary_footer_lines() {
        // Captured live from `winget upgrade --accept-source-agreements`:
        // trailing prose after the table (update count, pinned-package
        // notice) must not become a bogus "package".
        let sample = "\
Nome           ID                   Versão  Disponível Origem
-------------------------------------------------------------
Docker Desktop Docker.DockerDesktop 4.90.0  4.91.0     winget
flyctl         Fly-io.flyctl        0.4.102 0.4.103    winget
4 atualizações disponíveis.
1 pacote(s) têm pinos que impedem a atualização. Use o comando 'winget pin' para exibir e editar marcações. Usar o argumento '--include-pinned' pode mostrar mais resultados.
";
        let packages = parse_upgrade_table(sample);
        assert_eq!(
            packages.len(),
            2,
            "footer lines must not appear as packages"
        );
        assert_eq!(packages[0].name, "Docker Desktop");
        assert_eq!(packages[0].installed_version, "4.90.0");
        assert_eq!(packages[0].available_version, "4.91.0");
        assert_eq!(packages[1].name, "flyctl");
    }

    #[test]
    fn test_parse_winget_table_row_shorter_than_header_does_not_panic() {
        // Trailing columns are routinely stripped of trailing whitespace by
        // the terminal/pipe, so a data line can be physically shorter than
        // the header it's aligned under — but real winget output still pads
        // up to wherever the next column would start (see the real captured
        // fixtures above), so there's a genuine multi-space run. (Built with
        // `format!`, not a literal with trailing spaces on a line, since
        // those tend to get silently stripped by editors/tools.)
        let header = "Name                  ID                    Version           Source";
        let sep = "-".repeat(header.len());
        let sample = format!("{header}\n{sep}\nShort\nPadded{}\n", " ".repeat(16));

        let packages = parse_winget_table(&sample);
        assert_eq!(
            packages.len(),
            1,
            "'Short' has no column padding at all — not a row"
        );
        assert_eq!(packages[0].name, "Padded");
        assert_eq!(packages[0].id, "");
        assert_eq!(packages[0].version, None);
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
    fn test_parse_upgrade_table() {
        let sample = "\
Name                  Id                    Version    Available  Source
--------------------------------------------------------------------------
Google Chrome         Google.Chrome         133.0      134.0      winget
";
        let packages = parse_upgrade_table(sample);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "Google Chrome");
        assert_eq!(packages[0].id, "Google.Chrome");
        assert_eq!(packages[0].installed_version, "133.0");
        assert_eq!(packages[0].available_version, "134.0");
        assert_eq!(packages[0].source.as_deref(), Some("winget"));
    }

    #[test]
    fn test_parse_upgrade_table_blank_id_does_not_leak_into_version() {
        let sample = "\
Name                  Id                    Version    Available  Source
--------------------------------------------------------------------------
Google Chrome         Google.Chrome         133.0      134.0      winget
Weird Local App                             1.0        2.0
";
        let packages = parse_upgrade_table(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[1].name, "Weird Local App");
        assert_eq!(packages[1].id, "", "no Id column, not the version string");
        assert_eq!(packages[1].installed_version, "1.0");
        assert_eq!(packages[1].available_version, "2.0");
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
    fn test_install_args_omit_scope_and_locale_by_default() {
        // No `scope`/`locale` in the manifest -> let winget pick whatever
        // installer is actually applicable, instead of forcing --scope
        // machine (which fails outright for packages with no machine-scope
        // installer — reported as "winget install --exact ... --scope
        // machine ... not working, plain `winget install id --force` does").
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
        assert!(!args.iter().any(|a| a == "--scope"));
        assert!(!args.iter().any(|a| a == "--locale"));
    }

    #[test]
    fn test_install_args_uses_explicit_scope_when_given() {
        let pkg = JsonPackage {
            id: "Google.Chrome".to_string(),
            name: "Google Chrome".to_string(),
            command: None,
            is_script: false,
            args: Vec::new(),
            scope: Some("machine".to_string()),
            locale: None,
        };
        let args = pkg.install_args();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--scope" && w[1] == "machine")
        );
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

    #[test]
    fn parse_windows_update_json_empty_array_is_ok() {
        assert_eq!(parse_windows_update_json("[]").unwrap(), vec![]);
    }

    #[test]
    fn parse_windows_update_json_one_item() {
        let items = parse_windows_update_json(
            r#"[{"KB":"KB5000001","Title":"Cumulative Update","Size":"450 MB"}]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kb.as_deref(), Some("KB5000001"));
        assert_eq!(items[0].title, "Cumulative Update");
        assert_eq!(items[0].size.as_deref(), Some("450 MB"));
    }

    #[test]
    fn parse_windows_update_json_several_items() {
        let items = parse_windows_update_json(
            r#"[{"KB":"KB1","Title":"One","Size":"1 MB"},{"KB":"KB2","Title":"Two","Size":"2 MB"}]"#,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].kb.as_deref(), Some("KB2"));
    }

    #[test]
    fn parse_windows_update_json_missing_fields_uses_defaults() {
        let items = parse_windows_update_json(r#"[{"Title":"No KB or size"}]"#).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kb, None);
        assert_eq!(items[0].title, "No KB or size");
        assert_eq!(items[0].size, None);
    }

    #[test]
    fn parse_windows_update_json_malformed_input_errs() {
        assert!(parse_windows_update_json("not json").is_err());
        assert!(
            parse_windows_update_json("{}").is_err(),
            "a bare object (not an array) must not silently become an empty list"
        );
    }

    #[test]
    fn windows_update_check_script_wraps_selectobject_in_array_context() {
        let path = std::path::Path::new("C:/tmp/wgtui-wu.json");
        let script = windows_update_check_script(path);
        assert!(
            script.contains("@(Get-WindowsUpdate"),
            "must wrap in @(...) so a single result still serializes as a JSON array: {script}"
        );
    }

    #[test]
    fn windows_update_check_script_passes_updates_via_input_object_not_pipeline() {
        // Regression (found in real testing on a machine with zero pending
        // updates): `$updates | ConvertTo-Json` pipes the array *element by
        // element* — when $updates is empty, zero objects flow through the
        // pipeline, so ConvertTo-Json receives no input at all and emits
        // nothing, leaving an empty file. read_windows_update_result then
        // fails to parse the empty string ("expected value at line 1 column
        // 1"). Passing the array via -InputObject binds it as a single
        // value, so ConvertTo-Json correctly emits "[]" for zero elements
        // too (this is on top of, not instead of, the @(...) wrap above,
        // which is still needed so $updates itself is always array-typed).
        let path = std::path::Path::new("C:/tmp/wgtui-wu.json");
        let script = windows_update_check_script(path);
        assert!(
            script.contains("ConvertTo-Json -InputObject $updates"),
            "must pass $updates via -InputObject, not pipe it in, or an empty \
             result set (system up to date) produces no output at all: {script}"
        );
        assert!(
            !script.contains("$updates | ConvertTo-Json"),
            "must not pipe $updates into ConvertTo-Json: {script}"
        );
    }

    #[test]
    fn windows_update_check_script_removes_stale_file_before_anything_else() {
        let path = std::path::Path::new("C:/tmp/wgtui-wu.json");
        let script = windows_update_check_script(path);
        let remove_idx = script.find("Remove-Item").expect("Remove-Item present");
        let import_idx = script.find("Import-Module").expect("Import-Module present");
        let get_idx = script
            .find("Get-WindowsUpdate")
            .expect("Get-WindowsUpdate present");
        assert!(
            remove_idx < import_idx && remove_idx < get_idx,
            "stale output must be deleted before anything that could fail, so a missing file always means \"check didn't complete\""
        );
    }

    #[test]
    fn read_windows_update_result_parses_file_written_by_check_script() {
        let (_dir, path) = write_tmp("wu.json", r#"[{"KB":"KB1","Title":"T","Size":"1 MB"}]"#);
        let items = read_windows_update_result(&path).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kb.as_deref(), Some("KB1"));
    }

    #[test]
    fn read_windows_update_result_missing_file_is_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let never_written = dir.path().join("never-written.json");
        let err = read_windows_update_result(&never_written).unwrap_err();
        assert!(!err.is_empty());
    }
}
