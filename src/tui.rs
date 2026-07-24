use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use wgtui::{
    UpgradablePackage, WingetPackage, list_installed, list_upgradable, run_winget_stdout,
    search_packages, upgrade_all_packages,
};

/// The active tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Updates,
    Search,
    Installed,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Updates, Tab::Search, Tab::Installed];
    const STATUS_BAR_STR: &str =
        "  [Tab] tabs  [← ↑→ ↓] navigate  [/] filter  [Space] select  [Enter] show  ";

    fn next(self) -> Self {
        match self {
            Tab::Updates => Tab::Search,
            Tab::Search => Tab::Installed,
            Tab::Installed => Tab::Updates,
        }
    }

    fn prev(self) -> Self {
        match self {
            Tab::Updates => Tab::Installed,
            Tab::Search => Tab::Updates,
            Tab::Installed => Tab::Search,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Updates => "[1] Updates ",
            Tab::Search => "[2] Search ",
            Tab::Installed => "[3] Installed ",
        }
    }
}

/// A message sent back from a background thread when a blocking winget command finishes.
enum ActionResult {
    SearchResults(Vec<WingetPackage>),
    UpgradeList(Vec<UpgradablePackage>),
    SetCommand { command: String, output: String },
    SetError { command: String, error: String },
    RefreshInstalled(Vec<WingetPackage>),
    OutputLine(String),
    CommandDone,
}

/// Main application state.
pub struct App {
    pub tab: Tab,
    /// Whether the filter input is focused on the current tab.
    pub filter_focused: bool,
    /// Filter query text (shared across tabs, cleared on switch).
    pub filter_query: String,
    /// Results from the last winget search (unfiltered).
    pub search_results: Vec<WingetPackage>,
    /// Index in the search results list.
    pub search_selected: usize,
    /// Multi-selected indices in the search results list.
    search_selected_set: HashSet<usize>,
    /// Packages with available updates from `winget upgrade` (list mode).
    pub updates: Vec<UpgradablePackage>,
    /// Index in the updates list.
    pub updates_selected: usize,
    /// Multi-selected indices in the updates list.
    updates_selected_set: HashSet<usize>,
    /// Currently loaded installed packages (unfiltered).
    pub installed: Vec<WingetPackage>,
    /// Index in the installed list.
    pub installed_selected: usize,
    /// Multi-selected indices in the installed list.
    installed_selected_set: HashSet<usize>,
    /// The last winget command that was run (shown in the command bar).
    pub current_command: Option<String>,
    /// Output lines from the last command (shown in the output panel).
    pub command_output: Vec<String>,
    /// True while a blocking winget command is running.
    pub busy: bool,
    /// Cycles 0..3 for the spinner animation.
    pub spinner_frame: u8,
    /// Sender for background thread results.
    action_tx: mpsc::Sender<ActionResult>,
    /// Receiver for background thread results.
    action_rx: Receiver<ActionResult>,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        let installed = list_installed();
        let updates = list_upgradable();
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Updates,
            filter_focused: true,
            filter_query: String::new(),
            search_results: vec![],
            search_selected: 0,
            search_selected_set: HashSet::new(),
            updates,
            updates_selected: 0,
            updates_selected_set: HashSet::new(),
            installed,
            installed_selected: 0,
            installed_selected_set: HashSet::new(),
            current_command: None,
            command_output: vec![],
            busy: false,
            spinner_frame: 0,
            action_tx: tx,
            action_rx: rx,
            should_quit: false,
        }
    }

    /// Run the main event loop.
    pub fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            terminal.draw(|f| self.render(f))?;

            if self.should_quit {
                break;
            }

            // Drain completed background actions
            loop {
                match self.action_rx.try_recv() {
                    Ok(action) => self.handle_action_result(action),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.should_quit = true;
                        break;
                    }
                }
            }

            // Advance spinner and poll keyboard
            if self.busy {
                self.spinner_frame = (self.spinner_frame + 1) % 4;
            }
            if event::poll(Duration::from_millis(100))? {
                let event = event::read()?;
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    self.handle_key(key);
                }
            }
        }
        Ok(())
    }

    fn handle_action_result(&mut self, action: ActionResult) {
        match action {
            ActionResult::SearchResults(list) => {
                self.search_results = list;
                self.search_selected = 0;
            }
            ActionResult::UpgradeList(list) => {
                self.updates = list;
            }
            ActionResult::SetCommand { command, output } => {
                self.current_command = Some(command);
                self.command_output = output.lines().map(|l| l.to_string()).collect();
            }
            ActionResult::SetError { command, error } => {
                self.current_command = Some(command);
                self.command_output = error.lines().map(|l| l.to_string()).collect();
            }
            ActionResult::RefreshInstalled(list) => {
                self.installed = list;
                self.installed_selected = 0;
                self.current_command = Some("winget list --refresh".to_string());
                self.command_output = vec!["Package list refreshed.".to_string()];
            }
            ActionResult::OutputLine(line) => {
                self.command_output.push(line);
            }
            ActionResult::CommandDone => {
                self.busy = false;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Filter helpers
    // -----------------------------------------------------------------------

    fn matches_filter(item: &str, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let lower = item.to_lowercase();
        let filter_lower = filter.to_lowercase();
        // Check if all filter chars appear in order (simple fuzzy)
        let chars = filter_lower.chars();
        let mut rest = lower.as_str();
        for c in chars {
            match rest.find(c) {
                Some(pos) => rest = &rest[pos + 1..],
                None => return false,
            }
        }
        true
    }

    fn filtered_search_results(&self) -> Vec<&WingetPackage> {
        self.search_results
            .iter()
            .filter(|p| Self::matches_filter(&p.name, &self.filter_query))
            .collect()
    }

    fn filtered_installed(&self) -> Vec<&WingetPackage> {
        self.installed
            .iter()
            .filter(|p| Self::matches_filter(&p.name, &self.filter_query))
            .collect()
    }

    fn filtered_updates(&self) -> Vec<&UpgradablePackage> {
        self.updates
            .iter()
            .filter(|p| Self::matches_filter(&p.name, &self.filter_query))
            .collect()
    }

    fn selected_line(text: String, selected: bool) -> ListItem<'static> {
        if selected {
            ListItem::new(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )))
        } else {
            ListItem::new(Line::from(Span::raw(text)))
        }
    }

    fn clamp_selected(&mut self) {
        let n = self.filtered_search_results().len();
        if self.search_selected >= n && n > 0 {
            self.search_selected = n - 1;
        }
        let u = self.filtered_updates().len();
        if self.updates_selected >= u && u > 0 {
            self.updates_selected = u - 1;
        }
        let m = self.filtered_installed().len();
        if self.installed_selected >= m && m > 0 {
            self.installed_selected = m - 1;
        }
    }

    fn selected_ids(&self) -> Vec<String> {
        match self.tab {
            Tab::Search => {
                let filtered = self.filtered_search_results();
                if self.search_selected_set.is_empty() {
                    filtered
                        .get(self.search_selected)
                        .map(|p| p.id.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.search_selected_set
                        .iter()
                        .filter_map(|&i| filtered.get(i).map(|p| p.id.clone()))
                        .collect()
                }
            }
            Tab::Updates => {
                let filtered = self.filtered_updates();
                if self.updates_selected_set.is_empty() {
                    filtered
                        .get(self.updates_selected)
                        .map(|p| p.id.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.updates_selected_set
                        .iter()
                        .filter_map(|&i| filtered.get(i).map(|p| p.id.clone()))
                        .collect()
                }
            }
            Tab::Installed => {
                let filtered = self.filtered_installed();
                if self.installed_selected_set.is_empty() {
                    filtered
                        .get(self.installed_selected)
                        .map(|p| p.id.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.installed_selected_set
                        .iter()
                        .filter_map(|&i| filtered.get(i).map(|p| p.id.clone()))
                        .collect()
                }
            }
        }
    }

    fn toggle_selection(&mut self) {
        match self.tab {
            Tab::Search => {
                if self.search_selected_set.contains(&self.search_selected) {
                    self.search_selected_set.remove(&self.search_selected);
                } else {
                    self.search_selected_set.insert(self.search_selected);
                }
            }
            Tab::Updates => {
                if self.updates_selected_set.contains(&self.updates_selected) {
                    self.updates_selected_set.remove(&self.updates_selected);
                } else {
                    self.updates_selected_set.insert(self.updates_selected);
                }
            }
            Tab::Installed => {
                if self
                    .installed_selected_set
                    .contains(&self.installed_selected)
                {
                    self.installed_selected_set.remove(&self.installed_selected);
                } else {
                    self.installed_selected_set.insert(self.installed_selected);
                }
            }
        }
    }

    fn clear_selections(&mut self) {
        self.search_selected_set.clear();
        self.updates_selected_set.clear();
        self.installed_selected_set.clear();
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        if self.busy {
            return;
        }
        // Ctrl+C to quit
        if key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            self.should_quit = true;
            return;
        }

        match key.code {
            KeyCode::Char('/') => {
                self.filter_focused = !self.filter_focused;
                if !self.filter_focused {
                    self.clamp_selected();
                }
            }
            KeyCode::Char(' ') if !self.filter_focused => {
                self.toggle_selection();
            }
            KeyCode::Char('1') => {
                self.tab = Tab::Updates;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Search;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Installed;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.tab = self.tab.prev();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Right | KeyCode::Tab => {
                self.tab = self.tab.next();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Esc => {
                if self.filter_focused {
                    self.should_quit = true;
                } else {
                    self.filter_focused = true;
                }
            }
            _ => match self.tab {
                Tab::Updates => self.handle_updates_key(key),
                Tab::Search => self.handle_search_key(key),
                Tab::Installed => self.handle_installed_key(key),
            },
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
                KeyCode::Enter => {
                    self.trigger_search();
                }
                KeyCode::Down => {
                    self.filter_focused = false;
                    self.clamp_selected();
                }
                KeyCode::Char(c) => {
                    self.filter_query.push(c);
                }
                KeyCode::Backspace => {
                    self.filter_query.pop();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up => {
                    if self.search_selected > 0 {
                        self.search_selected -= 1;
                    } else {
                        self.filter_focused = true;
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_search_results().len();
                    if n > 0 && self.search_selected + 1 < n {
                        self.search_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.show_multi_pkg(ids);
                    }
                }
                KeyCode::Char('i') => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.install_multi_pkg(ids);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_updates_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
                KeyCode::Enter => {
                    self.filter_focused = !self.filter_focused;
                    if !self.filter_focused {
                        self.clamp_selected();
                    }
                }
                KeyCode::Down => {
                    self.filter_focused = false;
                    self.clamp_selected();
                }
                KeyCode::Char(c) => {
                    self.filter_query.push(c);
                }
                KeyCode::Backspace => {
                    self.filter_query.pop();
                    self.clamp_selected();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up => {
                    if self.updates_selected > 0 {
                        self.updates_selected -= 1;
                    } else {
                        self.filter_focused = true;
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_updates().len();
                    if n > 0 && self.updates_selected + 1 < n {
                        self.updates_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.show_multi_pkg(ids);
                    }
                }
                KeyCode::Char('u') => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.upgrade_multi_pkg(ids);
                    }
                }
                KeyCode::Char('U') => {
                    self.upgrade_all();
                }
                _ => {}
            }
        }
    }

    fn handle_installed_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
                KeyCode::Enter => {
                    self.filter_focused = !self.filter_focused;
                    if !self.filter_focused {
                        self.clamp_selected();
                    }
                }
                KeyCode::Down => {
                    self.filter_focused = false;
                    self.clamp_selected();
                }
                KeyCode::Char(c) => {
                    self.filter_query.push(c);
                }
                KeyCode::Backspace => {
                    self.filter_query.pop();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up => {
                    if self.installed_selected > 0 {
                        self.installed_selected -= 1;
                    } else {
                        self.filter_focused = true;
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_installed().len();
                    if n > 0 && self.installed_selected + 1 < n {
                        self.installed_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.show_multi_pkg(ids);
                    }
                }
                KeyCode::Char('r') => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.remove_multi_pkg(ids);
                    }
                }
                KeyCode::Char('u') | KeyCode::Char('U') => {
                    let ids = self.selected_ids();
                    if !ids.is_empty() {
                        self.upgrade_multi_pkg(ids);
                    }
                }
                KeyCode::Char('R') => {
                    self.refresh_installed();
                }
                _ => {}
            }
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    fn trigger_search(&mut self) {
        if self.filter_query.is_empty() {
            self.search_results.clear();
            self.search_selected = 0;
            return;
        }
        let query = self.filter_query.clone();
        let tx = self.action_tx.clone();
        self.current_command = Some(format!("winget search \"{}\"", query));
        self.busy = true;
        thread::spawn(move || {
            let results = search_packages(&query);
            let cmd = format!("winget search \"{}\"", query);
            let mut output = String::new();
            for pkg in &results {
                output.push_str(&format!("{}  {}\n", pkg.name, pkg.id));
            }
            let _ = tx.send(ActionResult::SearchResults(results));
            let _ = tx.send(ActionResult::SetCommand {
                command: cmd,
                output: output.trim().to_string(),
            });
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn install_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.current_command = Some(format!("winget install {} packages", ids.len()));
        self.busy = true;
        thread::spawn(move || {
            for id in &ids {
                let _ = tx.send(ActionResult::OutputLine(format!("--- install {} ---", id)));
                let tx2 = tx.clone();
                let (string_tx, string_rx) = mpsc::channel::<String>();
                thread::spawn(move || {
                    while let Ok(line) = string_rx.recv() {
                        let _ = tx2.send(ActionResult::OutputLine(line));
                    }
                });
                let args = [
                    "install",
                    "--exact",
                    id,
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                    "--scope",
                    "machine",
                ];
                let _ = run_winget_stdout(&args, string_tx);
                let _ = tx.send(ActionResult::OutputLine(String::new()));
            }
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn show_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.current_command = Some(format!("winget show {} packages", ids.len()));
        self.busy = true;
        thread::spawn(move || {
            for id in &ids {
                let _ = tx.send(ActionResult::OutputLine(format!("--- {} ---", id)));
                let tx2 = tx.clone();
                let (string_tx, string_rx) = mpsc::channel::<String>();
                thread::spawn(move || {
                    while let Ok(line) = string_rx.recv() {
                        let _ = tx2.send(ActionResult::OutputLine(line));
                    }
                });
                let args = ["show", id, "--accept-source-agreements"];
                let _ = run_winget_stdout(&args, string_tx);
                let _ = tx.send(ActionResult::OutputLine(String::new()));
            }
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn upgrade_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.current_command = Some(format!("winget upgrade {} packages", ids.len()));
        self.busy = true;
        thread::spawn(move || {
            for id in &ids {
                let _ = tx.send(ActionResult::OutputLine(format!("--- upgrade {} ---", id)));
                let tx2 = tx.clone();
                let (string_tx, string_rx) = mpsc::channel::<String>();
                thread::spawn(move || {
                    while let Ok(line) = string_rx.recv() {
                        let _ = tx2.send(ActionResult::OutputLine(line));
                    }
                });
                let args = [
                    "upgrade",
                    "--exact",
                    id,
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ];
                let _ = run_winget_stdout(&args, string_tx);
                let _ = tx.send(ActionResult::OutputLine(String::new()));
            }
            let updates = list_upgradable();
            let _ = tx.send(ActionResult::UpgradeList(updates));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn remove_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.current_command = Some(format!("winget uninstall {} packages", ids.len()));
        self.busy = true;
        thread::spawn(move || {
            for id in &ids {
                let _ = tx.send(ActionResult::OutputLine(format!(
                    "--- uninstall {} ---",
                    id
                )));
                let tx2 = tx.clone();
                let (string_tx, string_rx) = mpsc::channel::<String>();
                thread::spawn(move || {
                    while let Ok(line) = string_rx.recv() {
                        let _ = tx2.send(ActionResult::OutputLine(line));
                    }
                });
                let args = [
                    "uninstall",
                    "--exact",
                    id,
                    "--silent",
                    "--accept-source-agreements",
                ];
                let _ = run_winget_stdout(&args, string_tx);
                let _ = tx.send(ActionResult::OutputLine(String::new()));
            }
            let list = list_installed();
            let _ = tx.send(ActionResult::RefreshInstalled(list));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn upgrade_all(&mut self) {
        let tx = self.action_tx.clone();
        self.current_command = Some("winget upgrade --all --include-unknown".to_string());
        self.busy = true;
        thread::spawn(move || {
            let cmd = "winget upgrade --all --include-unknown".to_string();
            match upgrade_all_packages() {
                Ok(msg) => {
                    let _ = tx.send(ActionResult::SetCommand {
                        command: cmd,
                        output: msg.clone(),
                    });
                    let updates = list_upgradable();
                    let _ = tx.send(ActionResult::UpgradeList(updates));
                }
                Err(msg) => {
                    let _ = tx.send(ActionResult::SetError {
                        command: cmd,
                        error: msg,
                    });
                }
            }
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn refresh_installed(&mut self) {
        let tx = self.action_tx.clone();
        self.current_command = Some("winget list --refresh".to_string());
        self.busy = true;
        thread::spawn(move || {
            let list = list_installed();
            let _ = tx.send(ActionResult::RefreshInstalled(list));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    fn render(&self, f: &mut Frame<'_>) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_tabs(f, chunks[0]);
        self.render_filter_bar(f, chunks[1]);

        // Split content area vertically: main content + terminal panel at bottom
        let content_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Percentage(30)])
            .split(chunks[2]);

        // Split terminal panel into command bar + output
        let term_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(content_chunks[1]);

        self.render_content(f, content_chunks[0]);
        self.render_command_bar(f, term_chunks[0]);
        self.render_terminal_output(f, term_chunks[1]);
        self.render_status_bar(f, chunks[3]);
    }

    fn render_tabs(&self, f: &mut Frame<'_>, area: Rect) {
        let titles: Vec<&str> = Tab::ALL.iter().map(|t| t.title().trim()).collect();
        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" wgtui ")
                    .title_alignment(Alignment::Center),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .select(self.tab as usize);
        f.render_widget(tabs, area);
    }

    fn render_filter_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let title = match self.tab {
            Tab::Updates => " Filter updates ",
            Tab::Search => " Search (Enter to query winget) ",
            Tab::Installed => " Filter installed ",
        };
        let (focused, msg) = match self.tab {
            Tab::Updates => (self.filter_focused, self.filter_query.as_str()),
            Tab::Search => (self.filter_focused, self.filter_query.as_str()),
            Tab::Installed => (self.filter_focused, self.filter_query.as_str()),
        };

        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let prefix = Span::styled("> ", border_style);
        let cursor = if focused {
            Span::styled("█", Style::default().fg(Color::Cyan))
        } else {
            Span::raw("")
        };
        let query = Span::raw(msg);
        let line = Line::from(vec![prefix, query, cursor]);

        let widget = Paragraph::new(Text::from(line)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(border_style),
        );
        f.render_widget(widget, area);
    }

    fn render_content(&self, f: &mut Frame<'_>, area: Rect) {
        match self.tab {
            Tab::Updates => self.render_updates_list(f, area),
            Tab::Search => self.render_search_results(f, area),
            Tab::Installed => self.render_installed_list(f, area),
        }
    }

    fn render_search_results(&self, f: &mut Frame<'_>, area: Rect) {
        let border_style = if !self.filter_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let items: Vec<ListItem> = if self.search_results.is_empty() {
            vec![ListItem::new("Type a query and press Enter to search")]
        } else {
            self.search_results
                .iter()
                .enumerate()
                .map(|(i, pkg)| {
                    let v = pkg.version.as_deref().unwrap_or("-");
                    let s = pkg.source.as_deref().unwrap_or("-");
                    Self::selected_line(
                        format!(" {}  {}  [{}]  ({})", pkg.name, pkg.id, v, s),
                        self.search_selected_set.contains(&i),
                    )
                })
                .collect()
        };
        let count = self.search_results.len();
        let title = if count > 0 {
            format!(" Results ({} found) ", count)
        } else {
            " Results ".to_string()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state = ListState::default().with_selected(
            if self.search_results.is_empty() || self.filter_focused {
                None
            } else {
                Some(self.search_selected)
            },
        );
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_updates_list(&self, f: &mut Frame<'_>, area: Rect) {
        let filtered = self.filtered_updates();
        let border_style = if !self.filter_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let count_info = if self.filter_query.is_empty() {
            format!(" Updates ({} available) ", self.updates.len())
        } else {
            format!(
                " Updates ({} / {} filtered) ",
                filtered.len(),
                self.updates.len()
            )
        };

        let items: Vec<ListItem> = if filtered.is_empty() {
            let msg = if self.updates.is_empty() {
                "All packages are up to date"
            } else {
                "No packages match the filter"
            };
            vec![ListItem::new(msg)]
        } else {
            filtered
                .iter()
                .enumerate()
                .map(|(i, pkg)| {
                    Self::selected_line(
                        format!(
                            " {}  {} -> {}",
                            pkg.name, pkg.installed_version, pkg.available_version
                        ),
                        self.updates_selected_set.contains(&i),
                    )
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(count_info.as_str())
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state =
            ListState::default().with_selected(if filtered.is_empty() || self.filter_focused {
                None
            } else {
                Some(self.updates_selected)
            });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_installed_list(&self, f: &mut Frame<'_>, area: Rect) {
        let filtered = self.filtered_installed();
        let border_style = if !self.filter_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let count_info = if self.filter_query.is_empty() {
            format!(" Installed Packages ({} total) ", self.installed.len())
        } else {
            format!(
                " Installed Packages ({} / {} filtered) ",
                filtered.len(),
                self.installed.len()
            )
        };

        let items: Vec<ListItem> = if filtered.is_empty() {
            let msg = if self.installed.is_empty() {
                "No packages installed via winget"
            } else {
                "No packages match the filter"
            };
            vec![ListItem::new(msg)]
        } else {
            filtered
                .iter()
                .enumerate()
                .map(|(i, pkg)| {
                    let v = pkg.version.as_deref().unwrap_or("-");
                    Self::selected_line(
                        format!(" {}  [{}]", pkg.name, v),
                        self.installed_selected_set.contains(&i),
                    )
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(count_info.as_str())
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state =
            ListState::default().with_selected(if filtered.is_empty() || self.filter_focused {
                None
            } else {
                Some(self.installed_selected)
            });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_command_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let spinner = if self.busy {
            match self.spinner_frame {
                0 => ".",
                1 => "..",
                2 => ".",
                _ => " ",
            }
        } else {
            ""
        };
        let prompt = self
            .current_command
            .as_deref()
            .unwrap_or("waiting for command...");
        let line = Line::from(vec![
            Span::raw(spinner),
            Span::raw(" $ "),
            Span::raw(prompt),
        ]);
        f.render_widget(
            Paragraph::new(Text::from(line))
                .block(Block::default().borders(Borders::ALL).title(" Command ")),
            area,
        );
    }

    fn render_terminal_output(&self, f: &mut Frame<'_>, area: Rect) {
        let lines: Vec<Line> = self
            .command_output
            .iter()
            .map(|l| Line::from(Span::raw(l.as_str())))
            .collect();
        let text = if lines.is_empty() {
            Text::raw("")
        } else {
            Text::from(lines)
        };
        f.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Output ")),
            area,
        );
    }

    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let (left, right) = match self.tab {
            Tab::Updates => (
                Tab::STATUS_BAR_STR.to_owned() + "[u] upgrade  [U] update all  ",
                " [Esc] quit ",
            ),
            Tab::Search => (
                Tab::STATUS_BAR_STR.to_owned() + "[i] install  ",
                " [Esc] quit ",
            ),
            Tab::Installed => (
                Tab::STATUS_BAR_STR.to_owned() + "[u] upgrade  [r] remove  [R] refresh  ",
                " [Esc] quit ",
            ),
        };

        let padding = " ".repeat(
            area.width
                .saturating_sub(left.len() as u16 + right.len() as u16) as usize,
        );
        let line = Line::from(vec![
            Span::styled(left, Style::default().fg(Color::White).bg(Color::Blue)),
            Span::styled(&padding, Style::default().bg(Color::Blue)),
            Span::styled(right, Style::default().fg(Color::White).bg(Color::Blue)),
        ]);

        f.render_widget(Paragraph::new(Text::from(line)), area);
    }
}
