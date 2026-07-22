#![forbid(unsafe_code)]

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use colored::Colorize;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Common arguments passed to every PowerShell invocation by this tool.
const POWERSHELL_ARGS: &[&str] = &[
    "-NoProfile",
    "-InputFormat",
    "None",
    "-ExecutionPolicy",
    "Bypass",
    "-Command",
];

/// Base arguments for a Winget install command (the package identifier is appended).
const WINGET_INSTALL_ARGS: &[&str] = &[
    "--silent",
    "--accept-package-agreements",
    "--accept-source-agreements",
    "--scope",
    "machine",
];

/// Represents a package manager (Chocolatey, Scoop, Winget, or Custom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageManager {
    pub name: String,
    pub cli_install: Vec<String>,
    pub script: String,
    pub check_script: Vec<String>,
}

impl PackageManager {
    /// Returns `true` if this package manager is available on the system.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        if self.name == "Custom" {
            return false;
        }
        if self.check_script.is_empty() {
            return false;
        }

        let program = &self.check_script[0];
        let args = &self.check_script[1..];

        match Command::new(program).args(args).status() {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    }

    /// Downloads and installs the package manager via its PowerShell bootstrap script.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the PowerShell script fails to start or exits with a non-zero status.
    pub fn install(&self) -> Result<(), Error> {
        println!("{}", format!("Instalando {}...", self.name).bold().yellow());

        let status = Command::new("powershell")
            .args(POWERSHELL_ARGS)
            .arg(&self.script)
            .status()
            .map_err(Error::InstallScriptFailed)?;

        if !status.success() {
            return Err(Error::InstallStatusFailed {
                manager: self.name.clone(),
                status,
            });
        }

        if self.name == "Winget" {
            let _ = Command::new("winget").args(["source", "update"]).status();
        }

        println!(
            "{}",
            format!("{} instalado com sucesso!", self.name).green()
        );
        Ok(())
    }
}

/// Returns a [`PackageManager`] configured for Chocolatey.
#[must_use]
pub fn get_chocolatey() -> PackageManager {
    PackageManager {
        name: "Chocolatey".to_string(),
        cli_install: vec!["choco".to_string(), "install".to_string(), "-y".to_string()],
        script: r#"@powershell -NoProfile -ExecutionPolicy Bypass -Command "iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))" && SET "PATH=%PATH%;%ALLUSERSPROFILE%\chocolatey\bin""#.to_string(),
        check_script: vec!["choco".to_string(), "--version".to_string()],
    }
}

/// Returns a [`PackageManager`] configured for Scoop.
#[must_use]
pub fn get_scoop() -> PackageManager {
    PackageManager {
        name: "Scoop".to_string(),
        cli_install: vec!["scoop".to_string(), "install".to_string(), "-y".to_string()],
        script: r#"Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser; Invoke-RestMethod -Uri https://get.scoop.sh | Invoke-Expression"#.to_string(),
        check_script: vec!["scoop".to_string(), "--version".to_string()],
    }
}

/// Returns a [`PackageManager`] configured for Winget.
#[must_use]
pub fn get_winget() -> PackageManager {
    PackageManager {
        name: "Winget".to_string(),
        cli_install: std::iter::once("winget".to_string())
            .chain(std::iter::once("install".to_string()))
            .chain(WINGET_INSTALL_ARGS.iter().map(|s| s.to_string()))
            .collect(),
        script: r#"Install-PackageProvider -Name NuGet -MinimumVersion 2.8.5.201 -Force; Install-Module -Name Microsoft.Winget.Client -Force -AllowClobber"#.to_string(),
        check_script: vec!["winget".to_string(), "--version".to_string()],
    }
}

/// Returns a [`PackageManager`] configured for Custom (ad-hoc) commands.
#[must_use]
pub fn get_custom() -> PackageManager {
    PackageManager {
        name: "Custom".to_string(),
        cli_install: vec![],
        script: String::new(),
        check_script: vec![],
    }
}

/// A software package entry loaded from the JSON file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Package {
    /// Human-readable display name (e.g. "Google Chrome").
    pub name: String,
    /// Identifiers understood by the package manager, or raw command tokens for Custom.
    pub package_name: Vec<String>,
    /// Which manager owns this package: "Winget", "Chocolatey", "Scoop", or "Custom".
    pub package_manager: String,
    /// Populated at runtime by [`check_installed_packages`].
    #[serde(skip)]
    pub is_installed: bool,
}

impl Package {
    /// Returns a static reference to the [`PackageManager`] matching this package's type.
    #[must_use]
    pub fn get_manager(&self) -> &'static PackageManager {
        match self.package_manager.as_str() {
            "Chocolatey" => {
                static MGR: OnceLock<PackageManager> = OnceLock::new();
                MGR.get_or_init(get_chocolatey)
            }
            "Scoop" => {
                static MGR: OnceLock<PackageManager> = OnceLock::new();
                MGR.get_or_init(get_scoop)
            }
            "Winget" => {
                static MGR: OnceLock<PackageManager> = OnceLock::new();
                MGR.get_or_init(get_winget)
            }
            _ => {
                static MGR: OnceLock<PackageManager> = OnceLock::new();
                MGR.get_or_init(get_custom)
            }
        }
    }

    /// Returns the full command-line arguments needed to install this package.
    #[must_use]
    pub fn get_cmd(&self) -> Vec<String> {
        let manager = self.get_manager();
        manager
            .cli_install
            .iter()
            .chain(self.package_name.iter())
            .cloned()
            .collect()
    }

    /// Installs the package by running its command.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the command list is empty, fails to start, or exits with a non-zero status.
    pub fn install(&self) -> Result<(), Error> {
        println!(
            "{}",
            format!("Instalação/Comando \"{}\" iniciado...", self.name).bold()
        );

        let cmd_list = self.get_cmd();
        let needs_shell = cmd_list
            .first()
            .is_some_and(|first| first.contains('\\') || first.eq_ignore_ascii_case("powershell"));

        let status = if needs_shell {
            let joined = cmd_list.join(" ");
            Command::new("cmd").args(["/C", &joined]).status()?
        } else if cmd_list.is_empty() {
            return Err(Error::EmptyCommandList);
        } else {
            Command::new(&cmd_list[0]).args(&cmd_list[1..]).status()?
        };

        if !status.success() {
            return Err(Error::InstallFailedStatus(status));
        }

        println!(
            "{}",
            format!("Instalação/Comando \"{}\" finalizado!", self.name).bold()
        );
        Ok(())
    }
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

/// Checks which packages are already installed via Winget and updates their `is_installed` flag.
#[must_use]
pub fn check_installed_packages(mut packages: Vec<Package>) -> Vec<Package> {
    let re_spaces = {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"\s{2,}").expect("regex: two or more whitespace"))
    };

    let output = Command::new("winget")
        .args(["list", "--accept-source-agreements"])
        .output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            let mut header_index = None;

            for (i, line) in lines.iter().enumerate() {
                if (line.contains("Nome") || line.contains("Name")) && line.contains("ID") {
                    header_index = Some(i);
                    break;
                }
            }

            if let Some(header_idx) = header_index {
                let data_lines = &lines[header_idx + 1..];
                let values: Vec<Vec<String>> = data_lines
                    .iter()
                    .filter_map(|line| {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            return None;
                        }
                        let parts: Vec<String> =
                            re_spaces.split(trimmed).map(|s| s.to_string()).collect();
                        (!parts.is_empty()).then_some(parts)
                    })
                    .collect();

                for package in &mut packages {
                    if package.package_manager == "Custom" {
                        package.is_installed = false;
                        continue;
                    }

                    let installed = package.package_name.iter().any(|pkg_name| {
                        values.iter().any(|row| {
                            row.iter()
                                .any(|cell| cell.trim().eq_ignore_ascii_case(pkg_name.trim()))
                        })
                    });

                    package.is_installed = installed;
                }
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("Aviso: Não foi possível verificar pacotes instalados via Winget: {e}")
                    .yellow()
            );
        }
    }

    packages
}

/// Loads packages from a JSON file at `json_path`.
///
/// # Errors
///
/// Returns `Err` if the file cannot be read or its content is not valid JSON.
pub fn load_packages_from_json(json_path: &str) -> Result<Vec<Package>, Error> {
    let file = std::fs::File::open(json_path).map_err(Error::JsonReadError)?;
    let reader = std::io::BufReader::new(file);
    let packages: Vec<Package> = serde_json::from_reader(reader).map_err(Error::JsonParseError)?;
    Ok(packages)
}

/// Errors that can occur during package or manager installation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// PowerShell install script failed to launch.
    #[error("Falha ao executar script de instalação: {0}")]
    InstallScriptFailed(std::io::Error),

    /// Install subprocess exited with a non-zero status.
    #[error("O script de instalação do {manager} falhou com status {status}")]
    InstallStatusFailed {
        manager: String,
        status: std::process::ExitStatus,
    },

    /// Package install command failed to launch.
    #[error("Falha ao executar comando de instalação: {0}")]
    CommandFailed(#[from] std::io::Error),

    /// Package install subprocess exited with a non-zero status.
    #[error("Instalação falhou com status {0}")]
    InstallFailedStatus(std::process::ExitStatus),

    /// The command list resolved to an empty vector.
    #[error("Lista de comandos vazia")]
    EmptyCommandList,

    /// JSON file could not be read from disk.
    #[error("Falha ao ler arquivo JSON: {0}")]
    JsonReadError(std::io::Error),

    /// JSON content could not be parsed.
    #[error("Erro ao parsear JSON: {0}")]
    JsonParseError(#[from] serde_json::Error),
}
