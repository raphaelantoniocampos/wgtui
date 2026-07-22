use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};

use wgtui::{
    install_package, list_installed, search_packages, show_package, uninstall_package,
    upgrade_package, WingetPackage,
};

/// The active tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Installed,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Search, Tab::Installed];

    fn next(self) -> Self {
        match self {
            Tab::Search => Tab::Installed,
            Tab::Installed => Tab::Search,
        }
    }

    fn prev(self) -> Self {
        match self {
            Tab::Search => Tab::Installed,
            Tab::Installed => Tab::Search,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Tab::Search => " Search ",
            Tab::Installed => " Installed ",
        }
    }
}

/// Overlay screen state.
#[derive(Debug)]
pub enum Overlay {
    None,
    Progress { message: String, is_error: bool },
    ShowInfo { message: String },
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
    /// Currently loaded installed packages (unfiltered).
    pub installed: Vec<WingetPackage>,
    /// Index in the *filtered* installed list.
    pub installed_selected: usize,
    /// Overlay state.
    pub overlay: Overlay,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        let installed = list_installed();
        Self {
            tab: Tab::Search,
            filter_focused: true,
            filter_query: String::new(),
            search_results: vec![],
            search_selected: 0,
            installed,
            installed_selected: 0,
            overlay: Overlay::None,
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

            if event::poll(Duration::from_millis(200))? {
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

    fn clamp_selected(&mut self) {
        let n = self.filtered_search_results().len();
        if self.search_selected >= n && n > 0 {
            self.search_selected = n - 1;
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
        // Ctrl+C to quit
        if key.code == KeyCode::Char('c') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }

        match &self.overlay {
            Overlay::Progress { .. } => return,
            Overlay::ShowInfo { .. } => {
                if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                    self.overlay = Overlay::None;
                }
                return;
            }
            Overlay::None => {}
        }

        match key.code {
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::BackTab => {
                self.tab = self.tab.prev();
                self.filter_query.clear();
                self.filter_focused = true;
                self.clamp_selected();
            }
            KeyCode::Esc => {
                self.should_quit = true;
            }
            _ => match self.tab {
                Tab::Search => self.handle_search_key(key),
                Tab::Installed => self.handle_installed_key(key),
            },
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
                KeyCode::Enter => {
                    self.filter_focused = false;
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
                        self.install_pkg((*pkg).clone());
                    }
                }
                KeyCode::Esc => {
                    self.filter_focused = true;
                }
                KeyCode::Char('/') => {
                    self.filter_focused = true;
                }
                _ => {}
            }
        }
    }

    fn handle_installed_key(&mut self, key: KeyEvent) {
        if self.filter_focused {
            match key.code {
                KeyCode::Enter | KeyCode::Down => {
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
                KeyCode::Esc => {
                    self.filter_focused = true;
                }
                KeyCode::Char('/') => {
                    self.filter_focused = true;
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
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
        self.search_results = search_packages(&self.filter_query);
        self.search_selected = 0;
    }

    fn show_pkg(&mut self, pkg: WingetPackage) {
        let id = pkg.id.clone();
        self.overlay = Overlay::Progress {
            message: format!("Showing {}...\n\nPlease wait...", pkg.name),
            is_error: false,
        };
        match show_package(&id) {
            Ok(msg) => {
                self.overlay = Overlay::ShowInfo {
                    message: format!("{} ({})\n\n{}", pkg.name, pkg.id, msg),
                };
            }
            Err(msg) => {
                self.overlay = Overlay::ShowInfo {
                    message: format!("Failed to show {}:\n\n{}", pkg.name, msg),
                };
            }
        }
    }

    fn install_pkg(&mut self, pkg: WingetPackage) {
        let name = pkg.name.clone();
        let id = pkg.id.clone();
        self.overlay = Overlay::Progress {
            message: format!("Installing {}...\n\nPlease wait...", name),
            is_error: false,
        };
        match install_package(&id) {
            Ok(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Successfully installed {}!\n\n{}", name, msg),
                    is_error: false,
                };
            }
            Err(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Failed to install {}:\n\n{}", name, msg),
                    is_error: true,
                };
            }
        }
    }

    fn upgrade_pkg(&mut self, pkg: WingetPackage) {
        let name = pkg.name.clone();
        let id = pkg.id.clone();
        self.overlay = Overlay::Progress {
            message: format!("Upgrading {}...\n\nPlease wait...", name),
            is_error: false,
        };
        match upgrade_package(&id) {
            Ok(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Successfully upgraded {}!\n\n{}", name, msg),
                    is_error: false,
                };
            }
            Err(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Failed to upgrade {}:\n\n{}", name, msg),
                    is_error: true,
                };
            }
        }
    }

    fn remove_pkg(&mut self, pkg: WingetPackage) {
        let name = pkg.name.clone();
        let id = pkg.id.clone();
        self.overlay = Overlay::Progress {
            message: format!("Removing {}...\n\nPlease wait...", name),
            is_error: false,
        };
        match uninstall_package(&id) {
            Ok(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Successfully removed {}!\n\n{}", name, msg),
                    is_error: false,
                };
            }
            Err(msg) => {
                self.overlay = Overlay::Progress {
                    message: format!("Failed to remove {}:\n\n{}", name, msg),
                    is_error: true,
                };
            }
        }
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
        self.render_content(f, chunks[2]);
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
            Tab::Search => " Search (Enter to query winget) ",
            Tab::Installed => " Filter installed ",
        };
        let (focused, msg) = match self.tab {
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

        let widget = Paragraph::new(Text::from(line))
            .block(Block::default().borders(Borders::ALL).title(title).border_style(border_style));
        f.render_widget(widget, area);
    }

    fn render_content(&self, f: &mut Frame<'_>, area: Rect) {
        match self.tab {
            Tab::Search => self.render_search_results(f, area),
            Tab::Installed => self.render_installed_list(f, area),
        }

        // Overlays
        match &self.overlay {
            Overlay::None => {}
            Overlay::Progress { message, is_error } => {
                let title = if *is_error { " Error " } else { " Working " };
                self.render_overlay(f, area, title, message, *is_error);
            }
            Overlay::ShowInfo { message } => {
                self.render_overlay(f, area, " Package Info ", message, false);
            }
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
            .block(Block::default().borders(Borders::ALL).title(count_info.as_str()))
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

    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let (left, right) = match self.tab {
            Tab::Search => (
                " [Tab] switch  [/] filter  [↑↓] navigate  [Enter] install ",
                " [Esc] quit ",
            ),
            Tab::Installed => (
                " [Tab] switch  [/] filter  [↑↓] navigate  [Enter] show  [U] upgrade  [R] remove ",
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

    fn render_overlay(
        &self,
        f: &mut Frame<'_>,
        area: Rect,
        title: &str,
        message: &str,
        is_error: bool,
    ) {
        let border_color = if is_error { Color::Red } else { Color::Green };
        let popup_area = centered_rect(60, 40, area);

        let lines: Vec<Line> = message.lines().map(|l| Line::from(Span::raw(l))).collect();
        let paragraph = Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_color)
                    .title(format!(" {} ", title))
                    .title_alignment(Alignment::Center),
            )
            .wrap(Wrap { trim: true });

        f.render_widget(paragraph, popup_area);
    }
}

/// Helper to create a centered rect.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}