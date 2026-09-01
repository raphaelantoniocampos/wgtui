mod tui;

use crossterm::ExecutableCommand;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{Write, stdout};

use tui::App;
use wgtui::check_winget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !check_winget() {
        eprintln!("Winget not found. Please install winget first.");
        eprintln!("Recommend: Install-Module -Name Microsoft.Winget.Client -Force");
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
