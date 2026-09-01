mod bootstrap;
mod elevation;
mod tui;

use crossterm::ExecutableCommand;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{Write, stdout};

use bootstrap::{
    Bootstrap, ensure_winget, manual_instructions, prompt_yes_no, run_powershell_bootstrap,
};
use tui::App;
use wgtui::check_winget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let winget = ensure_winget(
        check_winget,
        || {
            eprintln!("winget não foi encontrado no PATH.");
            prompt_yes_no("Instalar o cliente winget agora via PowerShell?")
        },
        run_powershell_bootstrap,
    );
    if winget == Bootstrap::Unavailable {
        for line in manual_instructions() {
            eprintln!("{line}");
        }
        std::process::exit(1);
    }

    enable_raw_mode()?;
    let mut stdout = stdout();
    stdout.execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App::new();
    let result = app.run(&mut terminal);

    // Ensure terminal is restored even on error
    drop(terminal);
    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    std::io::stdout().flush()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }

    Ok(())
}
