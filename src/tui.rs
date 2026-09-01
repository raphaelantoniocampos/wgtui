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
    update_sources, upgrade_all_packages,
};

use crate::elevation::{elevation_warning, is_elevated};

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

/// Splits a command line into argv, honoring double-quoted groups.
///
/// No shell is involved when the command runs, so this only needs to group
/// tokens: `foo --bar "a b"` -> `["foo", "--bar", "a b"]`.
fn parse_command_line(line: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    argv.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        argv.push(cur);
    }
    argv
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
        "  [Tab/h l] tabs  [↑↓/j k] nav  [/] filter  [Space/a] select  [Enter] show  [c] cmd  ";

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
    /// Result of the startup admin-rights check.
    Elevation(bool),
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

/// Cursor + multi-selection state for one list.
///
/// Indices refer to the tab's **filtered** view, so navigation methods take the
/// current filtered length.
#[derive(Default)]
pub struct Selection {
    /// Cursor row.
    pub cursor: usize,
    /// Multi-selected rows. Empty means "act on the cursor row".
    pub marked: HashSet<usize>,
}

impl Selection {
    fn clamp(&mut self, len: usize) {
        if self.cursor >= len && len > 0 {
            self.cursor = len - 1;
        }
    }

    /// Moves the cursor up. Returns `true` if it was already at the top (the
    /// caller may then refocus the filter input).
    fn up(&mut self) -> bool {
        if self.cursor > 0 {
            self.cursor -= 1;
            false
        } else {
            true
        }
    }

    fn down(&mut self, len: usize) {
        if len > 0 && self.cursor + 1 < len {
            self.cursor += 1;
        }
    }

    fn toggle_current(&mut self) {
        if !self.marked.remove(&self.cursor) {
            self.marked.insert(self.cursor);
        }
    }

    /// Selects every row, or clears if all `len` rows are already selected.
    fn toggle_all(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        if self.marked.len() >= len {
            self.marked.clear();
        } else {
            self.marked = (0..len).collect();
        }
    }

    fn reset(&mut self) {
        self.cursor = 0;
        self.marked.clear();
    }

    /// Row indices to act on: the marked set (sorted), or `[cursor]` when
    /// nothing is marked.
    fn active(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            vec![self.cursor]
        } else {
            let mut v: Vec<usize> = self.marked.iter().copied().collect();
            v.sort_unstable();
            v
        }
    }
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
    /// Cursor / multi-selection in the search results list.
    pub search_sel: Selection,
    /// Packages with available updates from `winget upgrade` (list mode).
    pub updates: Vec<UpgradablePackage>,
    /// Cursor / multi-selection in the updates list.
    pub updates_sel: Selection,
    /// Currently loaded installed packages (unfiltered).
    pub installed: Vec<WingetPackage>,
    /// Cursor / multi-selection in the installed list.
    pub installed_sel: Selection,
    /// Discovered JSON package files (populated at startup).
    pub package_files: Vec<PathBuf>,
    /// Index in the file picker list.
    pub package_file_selected: usize,
    /// True when the file picker is shown (multiple JSON files found).
    packages_file_picker: bool,
    /// Packages loaded from a selected JSON file.
    pub packages: Vec<JsonPackage>,
    /// Cursor / multi-selection in the packages list.
    pub packages_sel: Selection,
    /// Diagnostic message about package loading (for debugging).
    packages_diagnostic: String,
    /// The last winget command that was run (shown in the command bar).
    pub current_command: Option<String>,
    /// The full command line last executed, verbatim — the seed for `[c]` edit.
    last_command: Option<String>,
    /// `Some` while the manual-command editor (`[c]`) is open.
    command_input: Option<String>,
    /// Output lines from the last command (shown in the output panel).
    pub command_output: Vec<String>,
    /// Scroll offset for terminal output (usize::MAX = auto-scroll to bottom).
    output_scroll: usize,
    /// True while a blocking winget command is running.
    pub busy: bool,
    /// Count of startup list loads (`winget list` + `winget upgrade`) not yet
    /// finished. Non-zero drives a "loading" spinner without blocking input.
    initial_load_pending: u8,
    /// Whether wgtui runs elevated. Assumed true until the startup check says
    /// otherwise, so no warning flashes on launch.
    elevated: bool,
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
            search_sel: Selection::default(),
            updates,
            updates_sel: Selection::default(),
            installed,
            installed_sel: Selection::default(),
            package_files,
            package_file_selected: 0,
            packages_file_picker,
            packages,
            packages_sel: Selection::default(),
            packages_diagnostic: diag,
            current_command: None,
            last_command: None,
            command_input: None,
            command_output: vec![],
            output_scroll: usize::MAX,
            busy: false,
            initial_load_pending: 2,
            elevated: true,
            spinner_frame: 0,
            action_tx: tx,
            action_rx: rx,
            should_quit: false,
        }
    }

    /// Kicks off the startup work on background threads: `winget list` /
    /// `winget upgrade` (results as `InitialInstalled` / `InitialUpdates`), the
    /// admin-rights check, and a silent `winget source update` that refreshes
    /// the upgrade list once the indexes are current.
    fn begin_initial_load(&self) {
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::InitialInstalled(list_installed()));
        });
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::InitialUpdates(list_upgradable()));
        });
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::Elevation(is_elevated()));
        });
        let tx = self.action_tx.clone();
        thread::spawn(move || {
            if update_sources() {
                let _ = tx.send(ActionResult::UpgradeList(list_upgradable()));
            }
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
                self.search_sel.cursor = 0;
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
            ActionResult::Elevation(elevated) => {
                self.elevated = elevated;
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
                self.installed_sel.cursor = 0;
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

    /// The current tab's selection state.
    fn sel(&self) -> &Selection {
        match self.tab {
            Tab::Search => &self.search_sel,
            Tab::Updates => &self.updates_sel,
            Tab::Installed => &self.installed_sel,
            Tab::Packages => &self.packages_sel,
        }
    }

    fn sel_mut(&mut self) -> &mut Selection {
        match self.tab {
            Tab::Search => &mut self.search_sel,
            Tab::Updates => &mut self.updates_sel,
            Tab::Installed => &mut self.installed_sel,
            Tab::Packages => &mut self.packages_sel,
        }
    }

    /// Number of rows visible in the current tab (after filtering).
    fn filtered_len(&self) -> usize {
        match self.tab {
            Tab::Search => self.filtered_search_results().len(),
            Tab::Updates => self.filtered_updates().len(),
            Tab::Installed => self.filtered_installed().len(),
            Tab::Packages => self.filtered_packages().len(),
        }
    }

    fn clamp_selected(&mut self) {
        let (s, u, i, p) = (
            self.filtered_search_results().len(),
            self.filtered_updates().len(),
            self.filtered_installed().len(),
            self.filtered_packages().len(),
        );
        self.search_sel.clamp(s);
        self.updates_sel.clamp(u);
        self.installed_sel.clamp(i);
        self.packages_sel.clamp(p);
    }

    fn selected_ids(&self) -> Vec<String> {
        let idx = self.sel().active();
        let pick = |ids: Vec<String>| -> Vec<String> {
            idx.iter().filter_map(|&i| ids.get(i).cloned()).collect()
        };
        match self.tab {
            Tab::Search => pick(
                self.filtered_search_results()
                    .iter()
                    .map(|p| p.id.clone())
                    .collect(),
            ),
            Tab::Updates => pick(
                self.filtered_updates()
                    .iter()
                    .map(|p| p.id.clone())
                    .collect(),
            ),
            Tab::Installed => pick(
                self.filtered_installed()
                    .iter()
                    .map(|p| p.id.clone())
                    .collect(),
            ),
            Tab::Packages => pick(
                self.filtered_packages()
                    .iter()
                    .map(|p| p.id.clone())
                    .collect(),
            ),
        }
    }

    fn toggle_selection(&mut self) {
        self.sel_mut().toggle_current();
    }

    fn clear_selections(&mut self) {
        for s in [
            &mut self.search_sel,
            &mut self.updates_sel,
            &mut self.installed_sel,
            &mut self.packages_sel,
        ] {
            s.marked.clear();
        }
    }

    /// Selects every visible row in the current tab, or clears if all are
    /// already selected.
    fn toggle_select_all(&mut self) {
        let n = self.filtered_len();
        self.sel_mut().toggle_all(n);
    }

    /// Switches to `tab`, resetting the shared filter and every tab's marks.
    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.filter_query.clear();
        self.filter_focused = false;
        self.clamp_selected();
        self.clear_selections();
    }

    /// `winget show` (or script details) for the current selection.
    fn show_selected(&mut self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        match self.tab {
            Tab::Packages => self.show_json_package(ids),
            _ => self.show_multi_pkg(ids),
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

        // Manual-command editor ([c]) captures all input while open.
        if self.command_input.is_some() {
            match key.code {
                KeyCode::Esc => self.command_input = None,
                KeyCode::Enter => {
                    if let Some(line) = self.command_input.take() {
                        self.run_manual_command(line);
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(buf) = self.command_input.as_mut() {
                        buf.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(buf) = self.command_input.as_mut() {
                        buf.pop();
                    }
                }
                _ => {}
            }
            return;
        }

        // vim: outside the filter input, `j`/`k` are list Down/Up. While the
        // filter is focused they are plain text (handled below).
        let key = if self.filter_focused {
            key
        } else {
            match key.code {
                KeyCode::Char('j') => KeyEvent::new(KeyCode::Down, key.modifiers),
                KeyCode::Char('k') => KeyEvent::new(KeyCode::Up, key.modifiers),
                _ => key,
            }
        };

        // Packages file picker has its own key handling logic first
        if self.tab == Tab::Packages && self.packages_file_picker {
            self.handle_packages_key(key);
            return;
        }

        if self.filter_focused {
            match key.code {
                KeyCode::Esc => {
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
            KeyCode::Char('a') => {
                self.toggle_select_all();
            }
            KeyCode::Char('c') => {
                self.command_input = Some(
                    self.last_command
                        .clone()
                        .unwrap_or_else(|| "winget ".to_string()),
                );
            }
            KeyCode::Char('1') => self.switch_tab(Tab::Updates),
            KeyCode::Char('2') => self.switch_tab(Tab::Search),
            KeyCode::Char('3') => self.switch_tab(Tab::Installed),
            KeyCode::Char('4') => self.switch_tab(Tab::Packages),
            KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => {
                self.switch_tab(self.tab.prev());
            }
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                self.switch_tab(self.tab.next());
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Up => {
                if self.sel_mut().up() {
                    self.filter_focused = true;
                }
            }
            KeyCode::Down => {
                let n = self.filtered_len();
                self.sel_mut().down(n);
            }
            KeyCode::Enter => self.show_selected(),
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
        if let KeyCode::Char('i') = key.code {
            let ids = self.selected_ids();
            if !ids.is_empty() {
                self.install_multi_pkg(ids);
            }
        }
    }

    fn handle_updates_key(&mut self, key: KeyEvent) {
        match key.code {
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
                        self.packages_sel.reset();
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
                    self.packages_sel.reset();
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
            self.search_sel.cursor = 0;
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
        if let Some(id) = ids.first() {
            self.set_last_command([
                "winget",
                "install",
                "--exact",
                id.as_str(),
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--scope",
                "machine",
            ]);
        }
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
        if let Some(id) = ids.first() {
            self.set_last_command([
                "winget",
                "upgrade",
                "--exact",
                id.as_str(),
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
            ]);
        }
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
        if let Some(id) = ids.first() {
            self.set_last_command([
                "winget",
                "uninstall",
                "--exact",
                id.as_str(),
                "--silent",
                "--accept-source-agreements",
            ]);
        }
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
        self.last_command = Some(
            "winget upgrade --all --include-unknown --silent \
             --accept-package-agreements --accept-source-agreements"
                .to_string(),
        );
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
        if let Some(seed) = ids
            .first()
            .and_then(|id| self.packages.iter().find(|p| &p.id == id))
            .map(|pkg| {
                if pkg.is_script {
                    pkg.command.clone().unwrap_or_default().join(" ")
                } else {
                    format!("winget {}", pkg.install_args().join(" "))
                }
            })
        {
            self.last_command = Some(seed);
        }
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

    /// Records the exact command line last run, so `[c]` can seed its editor.
    fn set_last_command<S: AsRef<str>>(&mut self, parts: impl IntoIterator<Item = S>) {
        let joined = parts
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        self.last_command = Some(joined);
    }

    /// Runs an arbitrary command line typed in the `[c]` editor, streaming its
    /// output, then refreshes the installed / upgrade lists.
    fn run_manual_command(&mut self, line: String) {
        let argv = parse_command_line(&line);
        if argv.is_empty() {
            return;
        }
        self.last_command = Some(line.clone());
        self.command_output.clear();
        self.output_scroll = usize::MAX;
        self.current_command = Some(line);
        self.busy = true;

        let tx = self.action_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(ActionResult::OutputLine(format!(
                "--- {} ---",
                argv.join(" ")
            )));
            let (string_tx, string_rx) = mpsc::channel::<String>();
            let tx2 = tx.clone();
            thread::spawn(move || {
                while let Ok(l) = string_rx.recv() {
                    let _ = tx2.send(ActionResult::OutputLine(l));
                }
            });
            let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
            let _ = wgtui::run_command_stdout(&argv[0], &args, string_tx);
            let _ = tx.send(ActionResult::RefreshInstalled(list_installed()));
            let _ = tx.send(ActionResult::UpgradeList(list_upgradable()));
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
                        self.search_sel.marked.contains(&i),
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
                Some(self.search_sel.cursor)
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
                        self.updates_sel.marked.contains(&i),
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
                Some(self.updates_sel.cursor)
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
                        self.installed_sel.marked.contains(&i),
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
                Some(self.installed_sel.cursor)
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
                    Self::selected_line(display, self.packages_sel.marked.contains(&i))
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
                Some(self.packages_sel.cursor)
            });
        f.render_stateful_widget(list, area, &mut state);
    }

    fn render_command_bar(&self, f: &mut Frame<'_>, area: Rect) {
        if let Some(input) = &self.command_input {
            let line = Line::from(vec![
                Span::styled(
                    " edit ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" $ "),
                Span::raw(input.as_str()),
                Span::styled("█", Style::default().fg(Color::Yellow)),
            ]);
            f.render_widget(
                Paragraph::new(Text::from(line)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(" Command  [Enter] run  [Esc] cancel "),
                ),
                area,
            );
            return;
        }

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

        let base = Style::default().fg(Color::White).bg(Color::Blue);
        let warn = elevation_warning(self.elevated).unwrap_or("");
        let warn_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        let used = left.len() + warn.len() + right.len();
        let padding = " ".repeat((area.width as usize).saturating_sub(used));
        let line = Line::from(vec![
            Span::styled(left, base),
            Span::styled(padding, base),
            Span::styled(warn, warn_style),
            Span::styled(right, base),
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
        app.installed_sel.cursor = 2;
        app.toggle_selection();
        assert!(app.installed_sel.marked.contains(&2));
        app.toggle_selection();
        assert!(!app.installed_sel.marked.contains(&2));
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

    #[test]
    fn elevation_defaults_true_and_updates_from_result() {
        let mut app = App::new();
        assert!(app.elevated, "assumed elevated until the check reports");
        app.handle_action_result(ActionResult::Elevation(false));
        assert!(!app.elevated);
    }

    #[test]
    fn upgrade_list_result_refreshes_updates_without_reloading() {
        // The startup `winget source update` thread feeds a fresh upgrade list
        // back through UpgradeList; it must not re-trigger the loading state.
        let mut app = App::new();
        app.initial_load_pending = 0;
        app.handle_action_result(ActionResult::UpgradeList(vec![upkg("a"), upkg("b")]));
        assert_eq!(app.updates.len(), 2);
        assert_eq!(app.initial_load_pending, 0);
    }

    // ----- vim keybindings -----

    fn ke(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn wpkg(name: &str) -> WingetPackage {
        WingetPackage {
            name: name.to_string(),
            id: format!("{name}.{name}"),
            version: None,
            source: None,
        }
    }

    fn upkg(name: &str) -> UpgradablePackage {
        UpgradablePackage {
            name: name.to_string(),
            id: format!("{name}.{name}"),
            installed_version: "1".into(),
            available_version: "2".into(),
            source: None,
        }
    }

    fn jpkg(name: &str) -> JsonPackage {
        JsonPackage {
            id: format!("{name}.{name}"),
            name: name.to_string(),
            command: None,
            is_script: false,
            args: Vec::new(),
            scope: None,
            locale: None,
        }
    }

    #[test]
    fn vim_jk_navigates_updates_installed_and_packages() {
        let mut app = App::new();

        app.updates = vec![upkg("a"), upkg("b"), upkg("c")];
        app.tab = Tab::Updates;
        app.handle_key(ke(KeyCode::Char('j')));
        app.handle_key(ke(KeyCode::Char('j')));
        assert_eq!(app.updates_sel.cursor, 2);
        app.handle_key(ke(KeyCode::Char('k')));
        assert_eq!(app.updates_sel.cursor, 1);

        app.installed = vec![wpkg("a"), wpkg("b")];
        app.tab = Tab::Installed;
        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('j')));
        assert_eq!(app.installed_sel.cursor, 1);

        app.packages = vec![jpkg("a"), jpkg("b"), jpkg("c")];
        app.tab = Tab::Packages;
        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('j')));
        assert_eq!(app.packages_sel.cursor, 1);
    }

    #[test]
    fn vim_jk_navigates_search_results() {
        let mut app = App::new();
        app.search_results = vec![wpkg("a"), wpkg("b")];
        app.tab = Tab::Search;
        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('j')));
        assert_eq!(app.search_sel.cursor, 1);
        app.handle_key(ke(KeyCode::Char('k')));
        assert_eq!(app.search_sel.cursor, 0);
    }

    #[test]
    fn vim_hl_switches_tabs_from_any_tab() {
        let mut app = App::new();
        app.tab = Tab::Updates;
        app.handle_key(ke(KeyCode::Char('l')));
        assert_eq!(app.tab, Tab::Search);
        app.handle_key(ke(KeyCode::Char('l')));
        assert_eq!(app.tab, Tab::Installed);
        app.handle_key(ke(KeyCode::Char('h')));
        assert_eq!(app.tab, Tab::Search);
    }

    #[test]
    fn q_quits_only_from_normal_mode_and_is_typable_in_filter() {
        let mut app = App::new();
        app.tab = Tab::Search;
        app.filter_focused = true;
        for c in "qbittorrent".chars() {
            app.handle_key(ke(KeyCode::Char(c)));
        }
        assert_eq!(app.filter_query, "qbittorrent");
        assert!(app.filter_focused);
        assert!(!app.should_quit);

        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn jk_are_typable_in_filter() {
        let mut app = App::new();
        app.tab = Tab::Installed;
        app.filter_focused = true;
        for c in "jetbrains".chars() {
            app.handle_key(ke(KeyCode::Char(c)));
        }
        assert_eq!(app.filter_query, "jetbrains");
    }

    #[test]
    fn parse_command_line_groups_quotes_and_trims() {
        assert_eq!(
            parse_command_line("  winget   list "),
            vec!["winget", "list"]
        );
        assert_eq!(
            parse_command_line(r#"winget install X --params "/Language:pt-BR""#),
            vec!["winget", "install", "X", "--params", "/Language:pt-BR"]
        );
        assert!(parse_command_line("   ").is_empty());
    }

    #[test]
    fn set_last_command_joins_argv() {
        let mut app = App::new();
        app.set_last_command(["winget", "uninstall", "--exact", "sharkdp.bat"]);
        assert_eq!(
            app.last_command.as_deref(),
            Some("winget uninstall --exact sharkdp.bat")
        );
    }

    #[test]
    fn c_opens_command_editor_seeded_with_last_command() {
        let mut app = App::new();
        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('c')));
        assert_eq!(app.command_input.as_deref(), Some("winget ")); // no prior command

        app.command_input = None;
        app.last_command = Some("winget uninstall --exact sharkdp.bat --silent".into());
        app.handle_key(ke(KeyCode::Char('c')));
        assert_eq!(
            app.command_input.as_deref(),
            Some("winget uninstall --exact sharkdp.bat --silent")
        );
    }

    #[test]
    fn command_editor_typing_backspace_and_cancel() {
        let mut app = App::new();
        app.command_input = Some("winget ".into());
        for c in "list".chars() {
            app.handle_key(ke(KeyCode::Char(c)));
        }
        assert_eq!(app.command_input.as_deref(), Some("winget list"));
        app.handle_key(ke(KeyCode::Backspace));
        assert_eq!(app.command_input.as_deref(), Some("winget lis"));
        app.handle_key(ke(KeyCode::Esc));
        assert!(app.command_input.is_none());
    }

    #[test]
    fn c_is_plain_text_while_the_filter_is_focused() {
        let mut app = App::new();
        app.tab = Tab::Search;
        app.filter_focused = true;
        for c in "cpuz".chars() {
            app.handle_key(ke(KeyCode::Char(c)));
        }
        assert_eq!(app.filter_query, "cpuz");
        assert!(app.command_input.is_none());
    }

    #[test]
    fn vim_jk_in_file_picker() {
        let mut app = App::new();
        app.tab = Tab::Packages;
        app.packages_file_picker = true;
        app.package_files = vec![PathBuf::from("a.json"), PathBuf::from("b.json")];
        app.filter_focused = false;
        app.handle_key(ke(KeyCode::Char('j')));
        assert_eq!(app.package_file_selected, 1);
        app.handle_key(ke(KeyCode::Char('k')));
        assert_eq!(app.package_file_selected, 0);
    }

    #[test]
    fn a_toggles_select_all_in_current_tab() {
        let mut app = App::new();
        app.installed = vec![wpkg("a"), wpkg("b"), wpkg("c")];
        app.tab = Tab::Installed;
        app.filter_focused = false;

        app.handle_key(ke(KeyCode::Char('a')));
        assert_eq!(app.installed_sel.marked.len(), 3);
        // second press clears
        app.handle_key(ke(KeyCode::Char('a')));
        assert!(app.installed_sel.marked.is_empty());
    }

    #[test]
    fn a_select_all_respects_the_filter() {
        let mut app = App::new();
        app.packages = vec![jpkg("alpha"), jpkg("beta"), jpkg("alpine")];
        app.tab = Tab::Packages;
        app.filter_query = "alp".to_string(); // matches alpha, alpine
        app.filter_focused = false;

        app.handle_key(ke(KeyCode::Char('a')));
        assert_eq!(app.packages_sel.marked.len(), 2);
    }

    #[test]
    fn a_is_typable_in_filter() {
        let mut app = App::new();
        app.tab = Tab::Search;
        app.filter_focused = true;
        for c in "anydesk".chars() {
            app.handle_key(ke(KeyCode::Char(c)));
        }
        assert_eq!(app.filter_query, "anydesk");
        assert!(app.search_sel.marked.is_empty());
    }
}
