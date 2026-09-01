use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use wgtui::{
    JsonPackage, UpgradablePackage, WingetPackage, find_package_json_files, is_installed,
    list_installed, list_upgradable, load_packages_from_file, run_winget_stdout, search_packages,
    upgrade_all_packages,
};

/// Returns the directories to search for manifest JSON files, in priority order.
///
/// For each base (exe dir, project root during dev, cwd) both the base itself
/// and its `examples/` and `manifests/` subdirectories are searched.
fn detect_package_dirs() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        bases.push(exe_dir.clone());
        // Project root (parent of target/debug/) during dev.
        if let Some(root) = exe_dir.parent().and_then(|p| p.parent()) {
            bases.push(root.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    for base in bases {
        for cand in [base.join("examples"), base.join("manifests"), base] {
            if !dirs.contains(&cand) {
                dirs.push(cand);
            }
        }
    }
    dirs
}

/// Env var that surfaces the raw package-discovery diagnostic in the UI.
const DEBUG_ENV: &str = "WGTUI_DEBUG";

/// Lines shown in the Apps/Scripts pane when no manifest could be loaded.
///
/// With `debug` set and a non-empty `diagnostic`, the raw discovery trace is
/// shown; otherwise a short hint pointing at the README.
fn empty_packages_lines(diagnostic: &str, debug: bool) -> Vec<String> {
    if debug && !diagnostic.trim().is_empty() {
        return diagnostic.lines().map(str::to_string).collect();
    }
    vec![
        "Nenhum manifesto encontrado.".to_string(),
        "Coloque um packages.json na pasta do executável (veja o README).".to_string(),
        format!("Defina {DEBUG_ENV}=1 para ver os diretórios verificados."),
    ]
}

/// The active tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Updates,
    Search,
    Installed,
    Packages,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Updates, Tab::Search, Tab::Installed, Tab::Packages];
    const STATUS_BAR_STR: &str =
        "  [Tab] tabs  [← ↑→ ↓] navigate  [/] filter  [Space] select  [Enter] show  ";

    fn next(self) -> Self {
        match self {
            Tab::Updates => Tab::Search,
            Tab::Search => Tab::Installed,
            Tab::Installed => Tab::Packages,
            Tab::Packages => Tab::Updates,
        }
    }

    fn prev(self) -> Self {
        match self {
            Tab::Updates => Tab::Packages,
            Tab::Search => Tab::Updates,
            Tab::Installed => Tab::Search,
            Tab::Packages => Tab::Installed,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Updates => "[1] Updates ",
            Tab::Search => "[2] Search ",
            Tab::Installed => "[3] Installed ",
            Tab::Packages => "[4] Apps/Scripts ",
        }
    }
}

/// A message sent back from a background thread when a blocking winget command finishes.
enum ActionResult {
    SearchResults(Vec<WingetPackage>),
    UpgradeList(Vec<UpgradablePackage>),
    /// First `winget list` at startup (quiet: no command-bar / output noise).
    InitialInstalled(Vec<WingetPackage>),
    /// First `winget upgrade` at startup (quiet).
    InitialUpdates(Vec<UpgradablePackage>),
    SetCommand {
        command: String,
        output: String,
    },
    SetError {
        command: String,
        error: String,
    },
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
    /// Discovered JSON package files (populated at startup).
    pub package_files: Vec<PathBuf>,
    /// Index in the file picker list.
    pub package_file_selected: usize,
    /// True when the file picker is shown (multiple JSON files found).
    packages_file_picker: bool,
    /// Packages loaded from a selected JSON file.
    pub packages: Vec<JsonPackage>,
    /// Index in the packages list.
    pub packages_selected: usize,
    /// Multi-selected indices in the packages list.
    packages_selected_set: HashSet<usize>,
    /// Diagnostic message about package loading (for debugging).
    packages_diagnostic: String,
    /// The last winget command that was run (shown in the command bar).
    pub current_command: Option<String>,
    /// Output lines from the last command (shown in the output panel).
    pub command_output: Vec<String>,
    /// Scroll offset for terminal output (usize::MAX = auto-scroll to bottom).
    output_scroll: usize,
    /// True while a blocking winget command is running.
    pub busy: bool,
    /// Count of startup list loads (`winget list` + `winget upgrade`) not yet
    /// finished. Non-zero drives a "loading" spinner without blocking input.
    initial_load_pending: u8,
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
        // Package lists are loaded off-thread (see `begin_initial_load`) so the
        // first frame paints immediately.
        let installed = Vec::new();
        let updates = Vec::new();
        let (tx, rx) = mpsc::channel();

        // Discover JSON package files.
        let mut diag = String::new();
        let mut package_files: Vec<PathBuf> = Vec::new();
        for dir in &detect_package_dirs() {
            diag.push_str(&format!("dir: {}\n", dir.display()));
            let files = find_package_json_files(dir);
            diag.push_str(&format!("  -> {} files\n", files.len()));
            package_files.extend(files);
        }

        let (packages, packages_file_picker) = if package_files.len() == 1 {
            let pkgs = load_packages_from_file(&package_files[0]);
            diag.push_str(&format!(
                "loaded {} pkgs from {}\n",
                pkgs.len(),
                package_files[0].display()
            ));
            (pkgs, false)
        } else {
            if package_files.is_empty() {
                diag.push_str("no json files found\n");
            } else {
                diag.push_str(&format!("{} files found, pick one\n", package_files.len()));
            }
            (vec![], true)
        };

        Self {
            tab: Tab::Updates,
            filter_focused: false,
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
            package_files,
            package_file_selected: 0,
            packages_file_picker,
            packages,
            packages_selected: 0,
            packages_selected_set: HashSet::new(),
            packages_diagnostic: diag,
            current_command: None,
            command_output: vec![],
            output_scroll: usize::MAX,
            busy: false,
            initial_load_pending: 2,
            spinner_frame: 0,
            action_tx: tx,
            action_rx: rx,
            should_quit: false,
        }
    }

    /// Kicks off the startup `winget list` / `winget upgrade` on background
    /// threads. Results arrive as `InitialInstalled` / `InitialUpdates`.
    fn begin_initial_load(&self) {
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::InitialInstalled(list_installed()));
        });
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::InitialUpdates(list_upgradable()));
        });
    }

    /// Run the main event loop.
    pub fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.begin_initial_load();
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
            if self.busy || self.initial_load_pending > 0 {
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
            ActionResult::InitialInstalled(list) => {
                self.installed = list;
                self.initial_load_pending = self.initial_load_pending.saturating_sub(1);
            }
            ActionResult::InitialUpdates(list) => {
                self.updates = list;
                self.initial_load_pending = self.initial_load_pending.saturating_sub(1);
            }
            ActionResult::SetCommand { command, output } => {
                self.current_command = Some(command);
                self.command_output = output.lines().map(|l| l.to_string()).collect();
                self.output_scroll = usize::MAX;
            }
            ActionResult::SetError { command, error } => {
                self.current_command = Some(command);
                self.command_output = error.lines().map(|l| l.to_string()).collect();
                self.output_scroll = usize::MAX;
            }
            ActionResult::RefreshInstalled(list) => {
                self.installed = list;
                self.installed_selected = 0;
                self.current_command = Some("winget list --refresh".to_string());
                self.command_output = vec!["Package list refreshed.".to_string()];
                self.output_scroll = usize::MAX;
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
        let p = self.filtered_packages().len();
        if self.packages_selected >= p && p > 0 {
            self.packages_selected = p - 1;
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
            Tab::Packages => {
                let filtered = self.filtered_packages();
                if self.packages_selected_set.is_empty() {
                    filtered
                        .get(self.packages_selected)
                        .map(|p| p.id.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.packages_selected_set
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
            Tab::Packages => {
                if self.packages_selected_set.contains(&self.packages_selected) {
                    self.packages_selected_set.remove(&self.packages_selected);
                } else {
                    self.packages_selected_set.insert(self.packages_selected);
                }
            }
        }
    }

    fn clear_selections(&mut self) {
        self.search_selected_set.clear();
        self.updates_selected_set.clear();
        self.installed_selected_set.clear();
        self.packages_selected_set.clear();
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

        // Packages file picker has its own key handling logic first
        if self.tab == Tab::Packages && self.packages_file_picker {
            self.handle_packages_key(key);
            return;
        }

        if self.filter_focused {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.filter_focused = false;
                    self.clamp_selected();
                    return;
                }
                KeyCode::Enter => {
                    if self.tab == Tab::Search {
                        self.trigger_search();
                    } else {
                        self.filter_focused = false;
                        self.clamp_selected();
                    }
                    return;
                }
                KeyCode::Char(c) => {
                    self.filter_query.push(c);
                    self.clamp_selected();
                    return;
                }
                KeyCode::Backspace => {
                    self.filter_query.pop();
                    self.clamp_selected();
                    return;
                }
                // Allow tab navigation and arrows to pass through to the shortcut logic
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {}
                KeyCode::Up | KeyCode::Down => {
                    self.filter_focused = false;
                }
                _ => {
                    return;
                }
            }
        }

        match key.code {
            KeyCode::Char('/') => {
                self.filter_focused = true;
            }
            KeyCode::Char(' ') => {
                self.toggle_selection();
            }
            KeyCode::Char('1') => {
                self.tab = Tab::Updates;
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Search;
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Installed;
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Char('4') => {
                self.tab = Tab::Packages;
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                self.tab = self.tab.prev();
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                self.tab = self.tab.next();
                self.filter_query.clear();
                self.filter_focused = false;
                self.clamp_selected();
                self.clear_selections();
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::PageUp => {
                if !self.command_output.is_empty() {
                    self.output_scroll = if self.output_scroll == usize::MAX {
                        self.command_output.len().saturating_sub(1)
                    } else {
                        self.output_scroll.saturating_sub(5)
                    };
                }
            }
            KeyCode::PageDown => {
                if !self.command_output.is_empty() && self.output_scroll != usize::MAX {
                    let new_scroll = self.output_scroll + 5;
                    if new_scroll >= self.command_output.len() {
                        self.output_scroll = usize::MAX;
                    } else {
                        self.output_scroll = new_scroll;
                    }
                }
            }
            _ => match self.tab {
                Tab::Updates => self.handle_updates_key(key),
                Tab::Search => self.handle_search_key(key),
                Tab::Installed => self.handle_installed_key(key),
                Tab::Packages => self.handle_packages_key(key),
            },
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.search_selected > 0 {
                    self.search_selected -= 1;
                } else {
                    self.filter_focused = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
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

    fn handle_updates_key(&mut self, key: KeyEvent) {
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

    fn filtered_packages(&self) -> Vec<&JsonPackage> {
        if self.filter_query.is_empty() {
            self.packages.iter().collect()
        } else {
            self.packages
                .iter()
                .filter(|p| {
                    Self::matches_filter(&p.name, &self.filter_query)
                        || Self::matches_filter(&p.id, &self.filter_query)
                })
                .collect()
        }
    }

    fn handle_installed_key(&mut self, key: KeyEvent) {
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

    fn handle_packages_key(&mut self, key: KeyEvent) {
        // File picker mode: pick a JSON file first
        if self.packages_file_picker {
            match key.code {
                KeyCode::Up => {
                    if self.package_file_selected > 0 {
                        self.package_file_selected -= 1;
                    }
                }
                KeyCode::Down => {
                    if self.package_file_selected + 1 < self.package_files.len() {
                        self.package_file_selected += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(path) = self.package_files.get(self.package_file_selected) {
                        let pkgs = load_packages_from_file(path);
                        self.packages = pkgs;
                        self.packages_selected = 0;
                        self.packages_selected_set.clear();
                        self.packages_file_picker = false;
                        self.packages_diagnostic = format!(
                            "loaded {} pkgs from {}",
                            self.packages.len(),
                            path.display()
                        );
                        self.filter_focused = false;
                        self.filter_query.clear();
                    }
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Up => {
                if self.packages_selected > 0 {
                    self.packages_selected -= 1;
                } else {
                    self.filter_focused = true;
                }
            }
            KeyCode::Down => {
                let n = self.filtered_packages().len();
                if n > 0 && self.packages_selected + 1 < n {
                    self.packages_selected += 1;
                }
            }
            KeyCode::Enter => {
                let ids = self.selected_ids();
                if !ids.is_empty() {
                    self.show_json_package(ids);
                }
            }
            KeyCode::Char('i') => {
                let ids = self.selected_ids();
                if !ids.is_empty() {
                    self.install_json_multi(ids);
                }
            }
            KeyCode::Char('I') => {
                let all_ids: Vec<String> = self.packages.iter().map(|p| p.id.clone()).collect();
                if !all_ids.is_empty() {
                    self.install_json_multi(all_ids);
                }
            }
            KeyCode::Char('r') => {
                let ids = self.selected_ids();
                if !ids.is_empty() {
                    self.remove_json_multi(ids);
                }
            }
            KeyCode::Char('R') => {
                let all_ids: Vec<String> = self.packages.iter().map(|p| p.id.clone()).collect();
                if !all_ids.is_empty() {
                    self.remove_json_multi(all_ids);
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                if !self.packages_file_picker && self.package_files.len() > 1 {
                    self.packages_file_picker = true;
                    self.packages.clear();
                    self.packages_selected = 0;
                    self.packages_selected_set.clear();
                    self.filter_focused = false;
                    self.filter_query.clear();
                }
            }
            _ => {}
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
        self.output_scroll = usize::MAX;
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
            let _ = tx.send(ActionResult::RefreshInstalled(list_installed()));
            let updates = list_upgradable();
            let _ = tx.send(ActionResult::UpgradeList(updates));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn show_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.output_scroll = usize::MAX;
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
        self.output_scroll = usize::MAX;
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
            let _ = tx.send(ActionResult::RefreshInstalled(list_installed()));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn remove_multi_pkg(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.output_scroll = usize::MAX;
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
                    let _ = tx.send(ActionResult::RefreshInstalled(list_installed()));
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

    fn install_json_multi(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.output_scroll = usize::MAX;
        self.current_command = Some(format!("install/run {} apps/scripts", ids.len()));
        self.busy = true;
        let tx_clone = tx.clone();
        let packages_clone = self.packages.clone();
        thread::spawn(move || {
            for id in &ids {
                if let Some(pkg) = packages_clone.iter().find(|p| p.id == *id) {
                    if pkg.is_script {
                        let _ = tx.send(ActionResult::OutputLine(format!(
                            "--- run script: {} ---",
                            pkg.name
                        )));
                        if let Some(ref cmd_args) = pkg.command
                            && !cmd_args.is_empty()
                        {
                            let cmd = &cmd_args[0];
                            let args: Vec<&str> =
                                cmd_args[1..].iter().map(|s| s.as_str()).collect();
                            let tx2 = tx.clone();
                            let (string_tx, string_rx) = mpsc::channel::<String>();
                            thread::spawn(move || {
                                while let Ok(line) = string_rx.recv() {
                                    let _ = tx2.send(ActionResult::OutputLine(line));
                                }
                            });
                            let _ = wgtui::run_command_stdout(cmd, &args, string_tx);
                        }
                    } else {
                        let args = pkg.install_args();
                        let _ = tx.send(ActionResult::OutputLine(format!(
                            "--- winget {} ---",
                            args.join(" ")
                        )));
                        let tx2 = tx.clone();
                        let (string_tx, string_rx) = mpsc::channel::<String>();
                        thread::spawn(move || {
                            while let Ok(line) = string_rx.recv() {
                                let _ = tx2.send(ActionResult::OutputLine(line));
                            }
                        });
                        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                        let _ = run_winget_stdout(&arg_refs, string_tx);
                    }
                    let _ = tx.send(ActionResult::OutputLine(String::new()));
                }
            }
            let _ = tx_clone.send(ActionResult::RefreshInstalled(list_installed()));
            let updates = list_upgradable();
            let _ = tx_clone.send(ActionResult::UpgradeList(updates));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn remove_json_multi(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        let tx_refresh = tx.clone();
        self.command_output.clear();
        self.output_scroll = usize::MAX;
        self.current_command = Some(format!("uninstall/remove {} apps/scripts", ids.len()));
        self.busy = true;
        let packages_clone = self.packages.clone();
        thread::spawn(move || {
            for id in &ids {
                if let Some(pkg) = packages_clone.iter().find(|p| p.id == *id) {
                    if pkg.is_script {
                        let _ = tx.send(ActionResult::OutputLine(format!(
                            "--- remove/uninstall not supported for script: {} ---",
                            pkg.name
                        )));
                    } else {
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
                    }
                    let _ = tx.send(ActionResult::OutputLine(String::new()));
                }
            }
            let _ = tx_refresh.send(ActionResult::RefreshInstalled(list_installed()));
            let updates = list_upgradable();
            let _ = tx_refresh.send(ActionResult::UpgradeList(updates));
            let _ = tx.send(ActionResult::CommandDone);
        });
    }

    fn show_json_package(&mut self, ids: Vec<String>) {
        let tx = self.action_tx.clone();
        self.command_output.clear();
        self.output_scroll = usize::MAX;
        self.current_command = Some(format!("show {} apps/scripts", ids.len()));
        self.busy = true;
        let packages_clone = self.packages.clone();
        thread::spawn(move || {
            for id in &ids {
                if let Some(pkg) = packages_clone.iter().find(|p| p.id == *id) {
                    if pkg.is_script {
                        let _ = tx.send(ActionResult::OutputLine(format!(
                            "--- script: {} ---",
                            pkg.name
                        )));
                        if let Some(ref cmd_args) = pkg.command {
                            let _ = tx.send(ActionResult::OutputLine(format!(
                                "Command: {}",
                                cmd_args.join(" ")
                            )));
                        }
                    } else {
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
                    }
                    let _ = tx.send(ActionResult::OutputLine(String::new()));
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
            Tab::Packages => " Filter apps/scripts ",
        };
        let focused = self.filter_focused;
        let msg = self.filter_query.as_str();

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
            Tab::Packages => self.render_packages_list(f, area),
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
            let msg = if self.initial_load_pending > 0 {
                "Carregando..."
            } else if self.updates.is_empty() {
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
            let msg = if self.initial_load_pending > 0 {
                "Carregando..."
            } else if self.installed.is_empty() {
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

    fn render_packages_list(&self, f: &mut Frame<'_>, area: Rect) {
        // File picker: show available JSON files
        if self.packages_file_picker {
            let items: Vec<ListItem> = if self.package_files.is_empty() {
                vec![ListItem::new("No JSON package files found")]
            } else {
                self.package_files
                    .iter()
                    .map(|p| {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy())
                            .unwrap_or_default()
                            .to_string();
                        ListItem::new(name)
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Select a package file (Enter to load) ")
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");

            let mut state = ListState::default().with_selected(if self.package_files.is_empty() {
                None
            } else {
                Some(self.package_file_selected)
            });
            f.render_stateful_widget(list, area, &mut state);
            return;
        }

        // Normal package list render
        let filtered = self.filtered_packages();
        let border_style = if !self.filter_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let count_info = if self.filter_query.is_empty() {
            format!(" Apps/Scripts ({} total) ", self.packages.len())
        } else {
            format!(
                " Apps/Scripts ({} / {} filtered) ",
                filtered.len(),
                self.packages.len()
            )
        };

        let items: Vec<ListItem> = if filtered.is_empty() {
            if self.packages.is_empty() {
                empty_packages_lines(
                    &self.packages_diagnostic,
                    std::env::var_os(DEBUG_ENV).is_some(),
                )
                .into_iter()
                .map(ListItem::new)
                .collect()
            } else {
                vec![ListItem::new("No packages match the filter")]
            }
        } else {
            filtered
                .iter()
                .enumerate()
                .map(|(i, pkg)| {
                    let display = if pkg.is_script {
                        format!("▶ {}", pkg.name)
                    } else if is_installed(&pkg.id, &pkg.name, &self.installed) {
                        format!("✓ {}", pkg.name)
                    } else {
                        format!("  {}", pkg.name)
                    };
                    Self::selected_line(display, self.packages_selected_set.contains(&i))
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
                Some(self.packages_selected)
            });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_command_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let loading = self.initial_load_pending > 0;
        let spinner = if self.busy || loading {
            match self.spinner_frame {
                0 => ".",
                1 => "..",
                2 => ".",
                _ => " ",
            }
        } else {
            ""
        };
        let prompt = if loading && self.current_command.is_none() {
            "carregando listas de pacotes..."
        } else {
            self.current_command
                .as_deref()
                .unwrap_or("waiting for command...")
        };
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
        let total = lines.len();
        let height = area.height as usize;
        let scroll = if self.output_scroll == usize::MAX {
            total.saturating_sub(height)
        } else {
            self.output_scroll.min(total.saturating_sub(1))
        };

        let text = if lines.is_empty() {
            Text::raw("")
        } else {
            Text::from(lines)
        };
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" Output "))
                .scroll((scroll as u16, 0)),
            area,
        );
    }

    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let (left, right) = match self.tab {
            Tab::Updates => (
                Tab::STATUS_BAR_STR.to_owned() + "[u] upgrade  [U] upgrade all  ",
                " [q] quit ",
            ),
            Tab::Search => (
                Tab::STATUS_BAR_STR.to_owned() + "[i] install  ",
                " [q] quit ",
            ),
            Tab::Installed => (
                Tab::STATUS_BAR_STR.to_owned() + "[u] upgrade  [r] remove  [R] refresh  ",
                " [q] quit ",
            ),
            Tab::Packages => (
                Tab::STATUS_BAR_STR.to_owned()
                    + "[i] install/run  [I] install/run all  [r] remove  [R] remove all  [F] file  ",
                " [q] quit ",
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

#[cfg(test)]
mod tests {
    use super::*;

    const DIAG: &str = "dir: C:\\x\n  -> 0 files\nno json files found\n";

    #[test]
    fn empty_packages_lines_shows_hint_by_default() {
        let lines = empty_packages_lines(DIAG, false);
        assert!(lines.iter().any(|l| l.contains("README")));
        assert!(!lines.iter().any(|l| l.contains("0 files")));
    }

    #[test]
    fn empty_packages_lines_shows_diagnostic_in_debug() {
        let lines = empty_packages_lines(DIAG, true);
        assert!(lines.iter().any(|l| l.contains("0 files")));
    }

    #[test]
    fn empty_packages_lines_falls_back_when_diagnostic_blank() {
        let lines = empty_packages_lines("   \n  ", true);
        assert!(lines.iter().any(|l| l.contains("README")));
    }

    #[test]
    fn detect_package_dirs_covers_cwd_and_examples() {
        let dirs = detect_package_dirs();
        let cwd = std::env::current_dir().unwrap();
        assert!(dirs.contains(&cwd));
        assert!(dirs.contains(&cwd.join("examples")));
    }

    #[test]
    fn app_new_does_not_block_on_winget() {
        // `new()` must not shell out to winget: it returns immediately with
        // empty lists and the initial load still pending.
        let app = App::new();
        assert!(app.installed.is_empty());
        assert!(app.updates.is_empty());
        assert!(!app.busy);
        assert_eq!(app.initial_load_pending, 2);
    }

    #[test]
    fn matches_filter_is_fuzzy_and_case_insensitive() {
        assert!(App::matches_filter("Google Chrome", ""));
        assert!(App::matches_filter("Google Chrome", "gc"));
        assert!(App::matches_filter("Google Chrome", "GOOG"));
        assert!(!App::matches_filter("Google Chrome", "xyz"));
        assert!(!App::matches_filter("abc", "abcd"));
    }

    #[test]
    fn tab_navigation_is_reversible_and_cyclic() {
        for t in Tab::ALL {
            assert_eq!(t.next().prev(), t);
        }
        assert_eq!(Tab::Packages.next(), Tab::Updates);
        assert_eq!(Tab::Updates.prev(), Tab::Packages);
    }

    #[test]
    fn toggle_selection_adds_then_removes_current_row() {
        let mut app = App::new();
        app.tab = Tab::Installed;
        app.installed_selected = 2;
        app.toggle_selection();
        assert!(app.installed_selected_set.contains(&2));
        app.toggle_selection();
        assert!(!app.installed_selected_set.contains(&2));
    }

    #[test]
    fn initial_result_clears_pending_flag() {
        let mut app = App::new();
        app.handle_action_result(ActionResult::InitialInstalled(vec![WingetPackage {
            name: "X".into(),
            id: "X.Y".into(),
            version: None,
            source: None,
        }]));
        assert_eq!(app.initial_load_pending, 1);
        app.handle_action_result(ActionResult::InitialUpdates(vec![]));
        assert_eq!(app.initial_load_pending, 0);
        assert_eq!(app.installed.len(), 1);
    }
}
