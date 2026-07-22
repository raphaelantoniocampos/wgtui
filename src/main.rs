use std::io::BufRead;

use anyhow::{Context, Result, bail};
use clap::Parser;
use colored::*;
use inquire::{Confirm, MultiSelect};
use wgtui::{
    Package, PackageManager, check_installed_packages, check_winget, get_winget,
    load_packages_from_json,
};

#[derive(Parser, Debug)]
#[command(
    name = "AutoPkg-Windows",
    version = "0.1.0",
    about = "Ferramenta Automática de Pacotes Windows",
    long_about = None
)]
struct Args {
    /// Path to JSON file with packages (optional, looks for packages.json for default)
    json_path: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Resolve json path (either passed argument, or default packages.json in same dir or next to exe)
    let raw_json_path = match args.json_path {
        Some(ref path) => std::path::PathBuf::from(path),
        None => {
            let local_default = std::path::PathBuf::from("packages.json");
            if local_default.exists() {
                local_default
            } else if let Ok(mut exe_path) = std::env::current_exe() {
                exe_path.pop();
                let next_to_exe = exe_path.join("packages.json");
                if next_to_exe.exists() {
                    next_to_exe
                } else {
                    bail!(
                        "\nErro: Nenhum arquivo JSON foi especificado e 'packages.json' não foi encontrado no diretório atual ou do executável.\nUso: wgtui.exe <caminho_do_json>"
                    );
                }
            } else {
                bail!(
                    "\nErro: Nenhum arquivo JSON foi especificado e 'packages.json' não foi encontrado no diretório atual ou do executável.\nUso: wgtui.exe <caminho_do_json>"
                );
            }
        }
    };

    let absolute_json_path = std::fs::canonicalize(&raw_json_path)
        .with_context(|| format!("Erro ao localizar o arquivo JSON {:?}", raw_json_path))?;

    // Change to home directory to avoid permission issues
    if let Some(home) = dirs::home_dir() {
        std::env::set_current_dir(&home).ok();
    }

    let packages = load_packages_from_json(&absolute_json_path.to_string_lossy())
        .with_context(|| format!("Falha ao carregar JSON de {:?}", absolute_json_path))?;

    let winget_installed: bool = check_winget();
    if winget_installed {
        println!("{}", "Winget não encontrado.".bold().yellow());
        let confirm = Confirm::new("Deseja instalar o winget?")
            .with_default(true)
            .prompt()?;

        if !confirm {
            println!("{}", "Instalação cancelada.".yellow());
            return Ok(());
        }
        let winget_mgr = get_winget();
        winget_mgr.install().context("Erro ao instalar o Winget")?;
        println!("{}", "Reinicie o programa para continuar.".bold().green());
        return Ok(());
    }

    let packages = check_installed_packages(packages);
    tui_mode(packages)?;
    wait_for_enter();

    Ok(())
}

fn tui_mode(packages: Vec<Package>) -> Result<()> {
    let banner = r#"
    ╔══════════════════════════════════════════════╗
    ║         AutoPkg-Windows v0.1.0               ║
    ║    Gerenciador Automático de Pacotes         ║
    ╚══════════════════════════════════════════════╝
    "#;
    println!("{}", banner.cyan().bold());

    let mut choices: Vec<String> = Vec::new();
    let mut custom_start = None;

    for (i, pkg) in packages.iter().enumerate() {
        if pkg.package_manager == "Custom" && custom_start.is_none() {
            choices.push("--- Custom ---".to_string());
            custom_start = Some(i);
        }
        let status = if pkg.is_installed { " ✅" } else { "" };
        choices.push(format!("{}{}", pkg.name, status));
    }
    choices.push("Sair".to_string());

    let selected = match MultiSelect::new("Selecione os programas para instalar:", choices).prompt()
    {
        Ok(val) => val,
        Err(_) => {
            println!("\n{}", "Interrompido pelo usuário.".yellow());
            return Ok(());
        }
    };

    let selected: Vec<&str> = selected.iter().map(String::as_str).collect();
    if selected.contains(&"Sair") {
        println!("{}", "Saindo...".yellow());
        return Ok(());
    }

    // Strip checkmark and filter out separators
    let selected_names: Vec<&str> = selected
        .into_iter()
        .filter(|s| !s.starts_with("---"))
        .map(|s| s.trim_end_matches(" ✅"))
        .collect();

    let selected_packages: Vec<&Package> = packages
        .iter()
        .filter(|p| selected_names.contains(&p.name.as_str()))
        .collect();

    if selected_packages.is_empty() {
        eprintln!("Nenhum programa foi selecionado!");
        return Ok(());
    }

    let confirm = Confirm::new("Deseja realmente instalar os programas selecionados?")
        .with_default(true)
        .prompt()?;

    if !confirm {
        println!("{}", "Instalação cancelada.".yellow());
        return Ok(());
    }

    let mut missing_managers: Vec<&'static PackageManager> = Vec::new();
    for pkg in &selected_packages {
        let mgr = pkg.get_manager();
        if mgr.name == "Custom" {
            continue;
        }
        if !mgr.is_installed() && !missing_managers.iter().any(|m| m.name == mgr.name) {
            missing_managers.push(mgr);
        }
    }

    if !missing_managers.is_empty() {
        println!(
            "{}",
            "Instalando gerenciadores de pacotes necessários..."
                .bold()
                .cyan()
        );
        for mgr in &missing_managers {
            if let Err(e) = mgr.install() {
                eprintln!("{}", format!("Erro ao instalar {}: {e}", mgr.name).red());
            }
        }
        println!(
            "{}",
            "Gerenciadores instalados. Reinicie o programa para continuar."
                .bold()
                .green()
        );
        return Ok(());
    }

    println!("{}", "Instalando pacotes...".bold().cyan());
    for pkg in &selected_packages {
        if let Err(e) = pkg.install() {
            eprintln!("{}", format!("Erro ao instalar {}: {e}", pkg.name).red());
        }
    }

    println!("{}", "Todos os pacotes foram processados.".bold().green());
    Ok(())
}

fn wait_for_enter() {
    println!("\nPressione Enter para sair...");
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

