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
    UpgradablePackage, WingetPackage, list_installed, list_upgradable, search_packages,
    show_package, uninstall_package, upgrade_all_packages, upgrade_all_unknown, upgrade_package,
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
    const STATUS_BAR_STR: &str = " [Tab] change focus  [← ↑→ ↓] navigate  ";

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
    UpgradeList(Vec<UpgradablePackage>),
    SetCommand { command: String, output: String },
    SetError { command: String, error: String },
    RefreshInstalled(Vec<WingetPackage>),
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
    /// Index in the *filtered* search results list.
    pub search_selected: usize,
    /// Packages with available updates from `winget upgrade` (list mode).
    pub updates: Vec<UpgradablePackage>,
    /// Index in the *filtered* updates list.
    pub updates_selected: usize,
    /// Currently loaded installed packages (unfiltered).
    pub installed: Vec<WingetPackage>,
    /// Index in the *filtered* installed list.
    pub installed_selected: usize,
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
            updates,
            updates_selected: 0,
            installed,
            installed_selected: 0,
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
        }
        self.busy = false;
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
            KeyCode::Tab | KeyCode::BackTab => {
                self.filter_focused = !self.filter_focused;
                if !self.filter_focused {
                    self.clamp_selected();
                }
            }
            KeyCode::Char('1') => {
                self.tab = Tab::Updates;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Search;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Installed;
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::Left => {
                self.tab = self.tab.prev();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::Right => {
                self.tab = self.tab.next();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
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
                    let n = self.filtered_search_results().len();
                    if n > 0 && self.search_selected > 0 {
                        self.search_selected -= 1;
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_search_results().len();
                    if n > 0 && self.search_selected + 1 < n {
                        self.search_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let filtered = self.filtered_search_results();
                    if let Some(pkg) = filtered.get(self.search_selected) {
                        self.show_pkg((*pkg).clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_updates_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
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
                    let n = self.filtered_updates().len();
                    if n > 0 {
                        self.updates_selected = self.updates_selected.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_updates().len();
                    if n > 0 && self.updates_selected + 1 < n {
                        self.updates_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let filtered = self.filtered_updates();
                    if let Some(pkg) = filtered.get(self.updates_selected) {
                        self.upgrade_single_pkg((*pkg).clone());
                    }
                }
                KeyCode::Char('a') => {
                    self.upgrade_all();
                }
                KeyCode::Char('U') => {
                    self.upgrade_all_unknown();
                }
                _ => {}
            }
        }
    }

    fn handle_installed_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
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
                    let n = self.filtered_installed().len();
                    if n > 0 && self.installed_selected > 0 {
                        self.installed_selected -= 1;
                    }
                }
                KeyCode::Down => {
                    let n = self.filtered_installed().len();
                    if n > 0 && self.installed_selected + 1 < n {
                        self.installed_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let filtered = self.filtered_installed();
                    if let Some(pkg) = filtered.get(self.installed_selected) {
                        self.show_pkg((*pkg).clone());
                    }
                }
                KeyCode::Char('r') => {
                    let filtered = self.filtered_installed();
                    if let Some(pkg) = filtered.get(self.installed_selected) {
                        self.remove_pkg((*pkg).clone());
                    }
                }
                KeyCode::Char('u') | KeyCode::Char('U') => {
                    let filtered = self.filtered_installed();
                    if let Some(pkg) = filtered.get(self.installed_selected) {
                        self.upgrade_pkg((*pkg).clone());
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
            let _ = tx.send(ActionResult::SetCommand {
                command: cmd,
                output: output.trim().to_string(),
            });
        });
    }

    fn show_pkg(&mut self, pkg: WingetPackage) {
        let id = pkg.id.clone();
        let tx = self.action_tx.clone();
        self.current_command = Some(format!("winget show \"{}\"", id));
        self.busy = true;
        thread::spawn(move || {
            let cmd = format!("winget show \"{}\"", id);
            match show_package(&id) {
                Ok(msg) => {
                    let _ = tx.send(ActionResult::SetCommand {
                        command: cmd,
                        output: msg,
                    });
                }
                Err(msg) => {
                    let _ = tx.send(ActionResult::SetError {
                        command: cmd,
                        error: msg,
                    });
                }
            }
        });
    }

    fn upgrade_pkg(&mut self, pkg: WingetPackage) {
        let id = pkg.id.clone();
        let tx = self.action_tx.clone();
        self.current_command = Some(format!("winget upgrade \"{}\"", id));
        self.busy = true;
        thread::spawn(move || {
            let cmd = format!("winget upgrade \"{}\"", id);
            match upgrade_package(&id) {
                Ok(msg) => {
                    let _ = tx.send(ActionResult::SetCommand {
                        command: cmd,
                        output: msg,
                    });
                }
                Err(msg) => {
                    let _ = tx.send(ActionResult::SetError {
                        command: cmd,
                        error: msg,
                    });
                }
            }
        });
    }

    fn remove_pkg(&mut self, pkg: WingetPackage) {
        let id = pkg.id.clone();
        let tx = self.action_tx.clone();
        self.current_command = Some(format!("winget uninstall \"{}\"", id));
        self.busy = true;
        thread::spawn(move || {
            let cmd = format!("winget uninstall \"{}\"", id);
            match uninstall_package(&id) {
                Ok(msg) => {
                    let _ = tx.send(ActionResult::SetCommand {
                        command: cmd,
                        output: msg,
                    });
                }
                Err(msg) => {
                    let _ = tx.send(ActionResult::SetError {
                        command: cmd,
                        error: msg,
                    });
                }
            }
        });
    }

    fn upgrade_single_pkg(&mut self, pkg: UpgradablePackage) {
        let id = pkg.id.clone();
        let tx = self.action_tx.clone();
        self.current_command = Some(format!("winget upgrade \"{}\"", id));
        self.busy = true;
        thread::spawn(move || {
            let cmd = format!("winget upgrade \"{}\"", id);
            match upgrade_package(&id) {
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
        });
    }

    fn upgrade_all(&mut self) {
        let tx = self.action_tx.clone();
        self.current_command = Some("winget upgrade --all".to_string());
        self.busy = true;
        thread::spawn(move || {
            let cmd = "winget upgrade --all".to_string();
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
        });
    }

    fn upgrade_all_unknown(&mut self) {
        let tx = self.action_tx.clone();
        self.current_command = Some("winget upgrade --all --include-unknown".to_string());
        self.busy = true;
        thread::spawn(move || {
            let cmd = "winget upgrade --all --include-unknown".to_string();
            match upgrade_all_unknown() {
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
        });
    }

    fn refresh_installed(&mut self) {
        let tx = self.action_tx.clone();
        self.current_command = Some("winget list --refresh".to_string());
        self.busy = true;
        thread::spawn(move || {
            let list = list_installed();
            let _ = tx.send(ActionResult::RefreshInstalled(list));
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
        let titles: Vec<Line> = Tab::ALL
            .iter()
            .map(|t| {
                let selected = *t == self.tab;
                let text = t.title();
                if selected {
                    Line::from(Span::styled(
                        text,
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(Span::raw(text))
                }
            })
            .collect();

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
            .select(0);
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
        let filtered = self.filtered_search_results();
        let items: Vec<ListItem> = if filtered.is_empty() {
            let msg = if self.search_results.is_empty() {
                "Type a query and press Enter to search"
            } else {
                "No results match the filter"
            };
            vec![ListItem::new(msg)]
        } else {
            filtered
                .iter()
                .map(|pkg| {
                    let v = pkg.version.as_deref().unwrap_or("-");
                    let s = pkg.source.as_deref().unwrap_or("-");
                    ListItem::new(format!(" {}  {}  [{}]  ({})", pkg.name, pkg.id, v, s))
                })
                .collect()
        };

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Results "))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state = ListState::default().with_selected(if filtered.is_empty() {
            None
        } else {
            Some(self.search_selected)
        });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_updates_list(&self, f: &mut Frame<'_>, area: Rect) {
        let filtered = self.filtered_updates();
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
                .map(|pkg| {
                    ListItem::new(format!(
                        " {}  {}  -> {}",
                        pkg.name, pkg.installed_version, pkg.available_version
                    ))
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(count_info.as_str()),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state = ListState::default().with_selected(if filtered.is_empty() {
            None
        } else {
            Some(self.updates_selected)
        });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_installed_list(&self, f: &mut Frame<'_>, area: Rect) {
        let filtered = self.filtered_installed();
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
                .map(|pkg| {
                    let v = pkg.version.as_deref().unwrap_or("-");
                    ListItem::new(format!(" {}  [{}]", pkg.name, v))
                })
                .collect()
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(count_info.as_str()),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        let mut state = ListState::default().with_selected(if filtered.is_empty() {
            None
        } else {
            Some(self.installed_selected)
        });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_command_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let spinner = if self.busy {
            match self.spinner_frame {
                0 => "⠋",
                1 => "⠙",
                2 => "⠸",
                _ => "⠴",
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
                Tab::STATUS_BAR_STR.to_owned()
                    + "[Enter] upgrade  [u] update all  [U] update all+unknown ",
                " [Esc] quit ",
            ),
            Tab::Search => (
                Tab::STATUS_BAR_STR.to_owned() + "[Enter] install ",
                " [Esc] quit ",
            ),
            Tab::Installed => (
                Tab::STATUS_BAR_STR.to_owned()
                    + "[Enter] show  [u] upgrade  [r] remove  [R] refresh ",
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
