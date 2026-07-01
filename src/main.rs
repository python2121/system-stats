mod config;
mod git;

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Tabs},
};

use config::Config;
use git::{GitTree, GraphRow, RecentCommit, Scanner, spawn_scanner};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    // Top-of-screen tab bar. Left/Right cycle tabs; Down drops into the body.
    Tabs,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    GitStatus,
    Placeholder1,
    Placeholder2,
}

const TABS: [Tab; 3] = [Tab::GitStatus, Tab::Placeholder1, Tab::Placeholder2];
const TAB_LABELS: [&str; 3] = ["Git Status", "Placeholder 1", "Placeholder 2"];

impl Tab {
    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }
}

// Menus are a stack: hitting Esc on the tab bar pushes `Main`; picking
// Settings from Main pushes `Settings` on top; Esc pops one level. The
// enum + selected-index combo keeps each level tiny while still letting
// draw_menu / activate switch on the current level's identity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuKind {
    Main,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainItem {
    Settings,
    Exit,
}

const MAIN_ITEMS: [MainItem; 2] = [MainItem::Settings, MainItem::Exit];
const MAIN_LABELS: [&str; 2] = ["Settings", "Exit"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsItem {
    WatchDir,
}

const SETTINGS_ITEMS: [SettingsItem; 1] = [SettingsItem::WatchDir];
const SETTINGS_LABELS: [&str; 1] = ["Watch directory"];

// Small modal shown when the user hits Esc on the tab bar. Owns its own
// cursor so nav keys can drive it independently of the pane focus underneath.
struct Menu {
    kind: MenuKind,
    selected: usize,
}

impl Menu {
    fn main() -> Self { Self { kind: MenuKind::Main, selected: 0 } }
    fn settings() -> Self { Self { kind: MenuKind::Settings, selected: 0 } }

    fn labels(&self) -> &'static [&'static str] {
        match self.kind {
            MenuKind::Main => &MAIN_LABELS,
            MenuKind::Settings => &SETTINGS_LABELS,
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            MenuKind::Main => " Menu — Esc to close ",
            MenuKind::Settings => " Settings — Esc back ",
        }
    }
}

// Modal for picking / changing the watched directory. Non-cancelable on
// first launch (can_cancel = false) so we always end up with a config file
// on disk; cancelable when opened from Settings so Esc is a real out.
struct DirectoryPrompt {
    input: String,
    // Char position (not byte) — 0..=char_count. Kept unicode-safe so a
    // path with multi-byte chars doesn't panic on Backspace.
    cursor: usize,
    error: Option<String>,
    can_cancel: bool,
}

impl DirectoryPrompt {
    fn new(current: &Path, can_cancel: bool) -> Self {
        let input = config::display_path(current);
        let cursor = input.chars().count();
        Self { input, cursor, error: None, can_cancel }
    }

    fn char_count(&self) -> usize {
        self.input.chars().count()
    }

    fn byte_at(&self, char_pos: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_pos)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    fn insert(&mut self, ch: char) {
        let i = self.byte_at(self.cursor);
        self.input.insert(i, ch);
        self.cursor += 1;
        self.error = None;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let end = self.byte_at(self.cursor);
            let start = self.byte_at(self.cursor - 1);
            self.input.replace_range(start..end, "");
            self.cursor -= 1;
            self.error = None;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.char_count() {
            let start = self.byte_at(self.cursor);
            let end = self.byte_at(self.cursor + 1);
            self.input.replace_range(start..end, "");
            self.error = None;
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    fn home(&mut self) { self.cursor = 0; }
    fn end(&mut self) { self.cursor = self.char_count(); }

    fn clear_line(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.error = None;
    }

    // Ctrl-W: kill the whitespace-bounded word to the left of the cursor,
    // eating any trailing whitespace first (so hitting Ctrl-W twice on
    // "foo bar " nukes " " then "bar").
    fn kill_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        if i < self.cursor {
            let start = self.byte_at(i);
            let end = self.byte_at(self.cursor);
            self.input.replace_range(start..end, "");
            self.cursor = i;
            self.error = None;
        }
    }

    // Convert the current input into an absolute PathBuf suitable for saving.
    // Expands `~`, then anchors any leftover relative path to CWD so the
    // saved setting doesn't depend on where the app was launched from.
    fn resolved_path(&self) -> PathBuf {
        let expanded = config::expand_tilde(self.input.trim());
        if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&expanded))
                .unwrap_or(expanded)
        }
    }
}

const EVENT_POLL: Duration = Duration::from_millis(250);
const HEATMAP_WEEKS_MAX: usize = 27;

struct App {
    scanner: Scanner,
    // Persisted settings (currently just watch_dir). Kept in-memory so that
    // key handlers can compare and write without re-reading the file.
    config: Config,
    git_tree: Option<GitTree>,
    // None = overall "my activity" view. Some(i) = drilled into a specific repo.
    // Esc returns to None; the first arrow press from None enters Some(0).
    selected_repo: Option<usize>,
    // Cached commit graphs keyed by repo name. Cleared when a new scan arrives
    // so a deleted/added repo can't show stale lines.
    graph_cache: HashMap<String, Vec<GraphRow>>,
    // Which zone responds to Up/Down/Left/Right.
    focus: Focus,
    // Which top-level tab is active.
    selected_tab: Tab,
    // Scroll offset (in lines) for the graph pane on the left. Reset when the
    // selection changes so a new pane starts at the top.
    left_scroll: u16,
    // Content length and visible height of the graph pane, cached during draw
    // so that scroll input can clamp against the actual rendered size.
    left_content_lines: Cell<u16>,
    left_inner_height: Cell<u16>,
    // Modal menu stack. Empty when no menu is showing. Esc pops one level;
    // picking an item can push another level (e.g. Main → Settings).
    // Always renders the top-of-stack; lower levels are hidden behind it.
    menu_stack: Vec<Menu>,
    // When Some, the directory prompt is open and swallows all input. Sits
    // above the menu stack — closing it drops the user back onto whichever
    // menu was topmost, so Esc-out-of-prompt naturally returns to Settings.
    dir_prompt: Option<DirectoryPrompt>,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        let config = Config::load().unwrap_or_default();
        // No config file on disk ⇒ first launch. Force a directory pick
        // before the user can navigate anywhere else. The scanner still
        // spins up on the (default) path so results appear immediately if
        // the user just hits Enter to accept.
        let dir_prompt = if !Config::exists() {
            Some(DirectoryPrompt::new(&config.watch_dir, false))
        } else {
            None
        };
        let scanner = spawn_scanner(config.watch_dir.clone());
        Self {
            scanner,
            config,
            git_tree: None,
            selected_repo: None,
            graph_cache: HashMap::new(),
            focus: Focus::Tabs,
            selected_tab: Tab::GitStatus,
            left_scroll: 0,
            left_content_lines: Cell::new(0),
            left_inner_height: Cell::new(0),
            menu_stack: Vec::new(),
            dir_prompt,
            should_quit: false,
        }
    }

    fn drain_git_updates(&mut self) {
        let mut got_new = false;
        while let Ok(tree) = self.scanner.try_recv() {
            // Discard trees scanned from a stale root. Prevents a scan
            // that was in flight when the user changed watch_dir from
            // clobbering the "scanning …" state with results from the
            // old directory.
            if tree.root != self.config.watch_dir {
                continue;
            }
            self.git_tree = Some(tree);
            got_new = true;
        }
        if got_new {
            self.graph_cache.clear();
            self.clamp_selection();
            // Don't reset left_scroll — the user may be mid-pan in the graph.
            // Scroll only resets on selection change (see move_selection / Esc).
            // If the new graph is shorter than the current offset, the next
            // key press clamps via scroll_left_pane.
        }
    }

    fn clamp_selection(&mut self) {
        let n = self.git_tree.as_ref().map(|t| t.repos.len()).unwrap_or(0);
        match self.selected_repo {
            Some(_) if n == 0 => self.selected_repo = None,
            Some(i) if i >= n => self.selected_repo = Some(n - 1),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let Some(tree) = &self.git_tree else { return };
        let n = tree.repos.len();
        if n == 0 {
            return;
        }
        let next = match self.selected_repo {
            // First arrow press from the overall view drops us into the
            // top-most repo regardless of direction.
            None => 0,
            Some(i) => (i as i32 + delta).clamp(0, n as i32 - 1) as usize,
        };
        if self.selected_repo != Some(next) {
            self.left_scroll = 0;
        }
        self.selected_repo = Some(next);
    }

    fn scroll_left_pane(&mut self, delta: i32) {
        let max = self
            .left_content_lines
            .get()
            .saturating_sub(self.left_inner_height.get());
        let next = (self.left_scroll as i32 + delta).clamp(0, max as i32);
        self.left_scroll = next as u16;
    }

    fn cycle_tab(&mut self, delta: i32) {
        let n = TABS.len() as i32;
        let cur = self.selected_tab.index() as i32;
        let next = (cur + delta).clamp(0, n - 1) as usize;
        self.selected_tab = TABS[next];
    }

    // Entering the git-activity pane from tabs: land the cursor on the top repo
    // so there's a visible selection to navigate from.
    fn enter_right_pane(&mut self) {
        self.focus = Focus::Right;
        let has_repos = self
            .git_tree
            .as_ref()
            .map(|t| !t.repos.is_empty())
            .unwrap_or(false);
        if self.selected_repo.is_none() && has_repos {
            self.selected_repo = Some(0);
        }
    }

    // Lazily fetch + cache the commit graph for the selected repo so we only
    // shell out to git when the user actually navigates to it.
    fn ensure_graph_loaded(&mut self) {
        let Some(tree) = &self.git_tree else { return };
        let Some(idx) = self.selected_repo else { return };
        let Some(repo) = tree.repos.get(idx) else { return };
        if self.graph_cache.contains_key(&repo.name) {
            return;
        }
        let rows = git::graph(&repo.path);
        self.graph_cache.insert(repo.name.clone(), rows);
    }

    fn menu_move(&mut self, delta: i32) {
        let Some(top) = self.menu_stack.last_mut() else { return };
        let n = top.labels().len() as i32;
        if n == 0 { return; }
        let next = (top.selected as i32 + delta).clamp(0, n - 1);
        top.selected = next as usize;
    }

    fn menu_activate(&mut self) {
        // Snapshot kind + selected so the borrow checker lets us push a
        // new menu (also `&mut self`) inside the match arms.
        let Some(top) = self.menu_stack.last() else { return };
        let kind = top.kind;
        let selected = top.selected;
        match kind {
            MenuKind::Main => match MAIN_ITEMS[selected] {
                // Settings pushes rather than replaces: hitting Esc from
                // Settings should return here, not close the menu entirely.
                MainItem::Settings => self.menu_stack.push(Menu::settings()),
                MainItem::Exit => self.should_quit = true,
            },
            MenuKind::Settings => match SETTINGS_ITEMS[selected] {
                // Leave the Settings menu on the stack while the prompt is
                // open, so Esc-out-of-prompt drops back onto Settings.
                SettingsItem::WatchDir => {
                    self.dir_prompt = Some(DirectoryPrompt::new(
                        &self.config.watch_dir,
                        true,
                    ));
                }
            },
        }
    }

    // Commit the prompt: validate → write to disk → tell the scanner. If any
    // step fails we leave the prompt open with an error message so the user
    // can fix it in place.
    fn submit_dir_prompt(&mut self) {
        let Some(prompt) = self.dir_prompt.as_mut() else { return };
        let resolved = prompt.resolved_path();
        if !resolved.is_dir() {
            prompt.error = Some(format!("not a directory: {}", resolved.display()));
            return;
        }
        let same_path = resolved == self.config.watch_dir;
        // On first launch we always write the file even if the user just
        // accepted the default — otherwise the prompt would re-appear next
        // start. On later runs we skip the write when nothing changed.
        let needs_write = !Config::exists() || !same_path;
        if needs_write {
            let mut new_config = self.config.clone();
            new_config.watch_dir = resolved.clone();
            if let Err(e) = new_config.save() {
                if let Some(p) = self.dir_prompt.as_mut() {
                    p.error = Some(format!("failed to save config: {e}"));
                }
                return;
            }
            self.config = new_config;
        }
        // Only touch the scanner + repo state when the path actually moved.
        // A no-op accept shouldn't churn a rescan.
        if !same_path {
            self.scanner.set_root(resolved);
            self.git_tree = None;
            self.graph_cache.clear();
            self.selected_repo = None;
            self.left_scroll = 0;
            self.focus = Focus::Tabs;
        }
        self.dir_prompt = None;
    }

    fn handle_events(&mut self) -> std::io::Result<()> {
        if !event::poll(EVENT_POLL)? {
            return Ok(());
        }
        let Event::Key(key) = event::read()? else {
            return Ok(());
        };
        // Modals take precedence over the tab/pane input. The prompt sits
        // above the menu in priority so if we ever open both, the prompt
        // wins.
        if self.dir_prompt.is_some() {
            self.handle_prompt_key(key);
            return Ok(());
        }
        if !self.menu_stack.is_empty() {
            self.handle_menu_key(key);
            return Ok(());
        }
        self.handle_main_key(key);
        Ok(())
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            // Esc pops one level — Settings → Main → app.
            (KeyCode::Esc, _) => { self.menu_stack.pop(); }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.menu_move(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.menu_move(1),
            (KeyCode::Enter, _) => self.menu_activate(),
            _ => {}
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        // Snapshot cancelability up front so the borrow checker is happy
        // when we later reach for `&mut self.dir_prompt` inside arms.
        let can_cancel = self
            .dir_prompt
            .as_ref()
            .map(|p| p.can_cancel)
            .unwrap_or(false);
        let ctrl = KeyModifiers::CONTROL;
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), m) if m == ctrl => self.should_quit = true,
            (KeyCode::Esc, _) if can_cancel => self.dir_prompt = None,
            (KeyCode::Enter, _) => self.submit_dir_prompt(),
            (KeyCode::Backspace, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.backspace(); }
            }
            (KeyCode::Delete, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.delete(); }
            }
            (KeyCode::Left, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.move_left(); }
            }
            (KeyCode::Right, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.move_right(); }
            }
            (KeyCode::Home, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.home(); }
            }
            (KeyCode::End, _) => {
                if let Some(p) = self.dir_prompt.as_mut() { p.end(); }
            }
            // Readline-style bindings — expected reflexes for anyone who's
            // used bash/zsh/emacs. Ctrl-C is handled above and takes priority.
            (KeyCode::Char('a'), m) if m == ctrl => {
                if let Some(p) = self.dir_prompt.as_mut() { p.home(); }
            }
            (KeyCode::Char('e'), m) if m == ctrl => {
                if let Some(p) = self.dir_prompt.as_mut() { p.end(); }
            }
            (KeyCode::Char('u'), m) if m == ctrl => {
                if let Some(p) = self.dir_prompt.as_mut() { p.clear_line(); }
            }
            (KeyCode::Char('w'), m) if m == ctrl => {
                if let Some(p) = self.dir_prompt.as_mut() { p.kill_word_back(); }
            }
            (KeyCode::Char(c), m)
                if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(p) = self.dir_prompt.as_mut() { p.insert(c); }
            }
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => self.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Esc, _) => {
                if self.focus == Focus::Tabs {
                    self.menu_stack.push(Menu::main());
                } else {
                    self.selected_repo = None;
                    self.focus = Focus::Tabs;
                    self.left_scroll = 0;
                }
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), _) => match self.focus {
                Focus::Tabs => self.cycle_tab(-1),
                Focus::Right | Focus::Left => self.focus = Focus::Left,
            },
            (KeyCode::Right, _) | (KeyCode::Char('l'), _) => match self.focus {
                Focus::Tabs => self.cycle_tab(1),
                Focus::Right | Focus::Left => self.focus = Focus::Right,
            },
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => match self.focus {
                Focus::Tabs => {}
                Focus::Right => match self.selected_repo {
                    // At the top of the list (or no cursor yet) → jump to tabs.
                    None | Some(0) => {
                        self.focus = Focus::Tabs;
                        self.selected_repo = None;
                    }
                    Some(i) => {
                        self.selected_repo = Some(i - 1);
                        self.left_scroll = 0;
                    }
                },
                Focus::Left => {
                    if self.left_scroll == 0 {
                        self.focus = Focus::Tabs;
                    } else {
                        self.scroll_left_pane(-1);
                    }
                }
            },
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => match self.focus {
                Focus::Tabs => self.enter_right_pane(),
                Focus::Right => self.move_selection(1),
                Focus::Left => self.scroll_left_pane(1),
            },
            _ => {}
        }
    }
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    while !app.should_quit {
        app.drain_git_updates();
        app.ensure_graph_loaded();
        terminal.draw(|f| draw(f, app))?;
        app.handle_events()?;
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(f.area());

    draw_tabs(f, app, vertical[0]);

    match app.selected_tab {
        Tab::GitStatus => draw_git_status(f, app, vertical[1]),
        Tab::Placeholder1 | Tab::Placeholder2 => draw_placeholder(f, app, vertical[1]),
    }

    if !app.menu_stack.is_empty() {
        draw_menu(f, app);
    }
    // Drawn last so it sits above the menu; if both were ever open at
    // once the prompt wins (matches handle_events priority).
    if app.dir_prompt.is_some() {
        draw_directory_prompt(f, app);
    }
}

// Modal for picking / changing the watched directory. Renders a text
// input with a horizontally-scrolling window so long paths stay usable,
// plus optional error line and a keybinding hint.
fn draw_directory_prompt(f: &mut Frame, app: &App) {
    let Some(prompt) = &app.dir_prompt else { return };

    let full = f.area();
    // Cap the modal so it doesn't dominate a wide terminal but grows
    // wide enough to make long paths comfortable.
    let width = full.width.clamp(40, 72);
    let height: u16 = if prompt.error.is_some() { 9 } else { 7 };
    let area = centered_rect(width, height, full);
    f.render_widget(Clear, area);

    let title = if prompt.can_cancel {
        " Watch directory "
    } else {
        " Welcome — pick a directory to watch "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);

    let label = "Directory: ";
    let label_len = label.chars().count();
    // Space for the input inside the block, after the label. `saturating_sub`
    // handles a pathologically narrow terminal — width is clamped above but
    // if the label wouldn't fit, `input_area_width` collapses to 0 and the
    // input just doesn't render (still no panic).
    let inner_width = area.width.saturating_sub(2) as usize;
    let input_area_width = inner_width.saturating_sub(label_len);
    let (visible, cursor_offset) =
        input_window(&prompt.input, prompt.cursor, input_area_width);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::raw(visible),
    ]));
    if let Some(err) = &prompt.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    let hint = if prompt.can_cancel {
        "Enter to save · Esc to cancel"
    } else {
        "Enter to save · Ctrl-C to quit"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));

    let para = Paragraph::new(lines).style(Style::reset()).block(block);
    f.render_widget(para, area);

    // Position the terminal cursor at the input caret so it blinks in the
    // right spot. Line 0 (inside the block) is blank; input sits on line 1.
    if input_area_width > 0 {
        let cursor_x = area.x + 1 + label_len as u16 + cursor_offset as u16;
        let cursor_y = area.y + 2;
        f.set_cursor_position(Position { x: cursor_x, y: cursor_y });
    }
}

// Compute a (visible_slice, cursor_within_slice) pair that keeps the
// caret inside a fixed-width window. Reserves one column for the trailing
// caret position (when the cursor is at the end of the input) so it never
// paints on the block border.
fn input_window(input: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    // Reserve the last column for a caret sitting at end-of-input.
    let cap = width - 1;
    if cap == 0 {
        return (String::new(), 0);
    }
    let chars: Vec<char> = input.chars().collect();
    let count = chars.len();
    if count <= cap {
        return (input.to_string(), cursor.min(count));
    }
    // Keep the caret a few cells from the left edge when scrolling right,
    // so users see some context to the left of where they're typing.
    let margin = 2usize.min(cap / 4);
    let ideal_start = cursor.saturating_sub(margin);
    let max_start = count - cap;
    let start = ideal_start.min(max_start);
    let visible: String = chars[start..start + cap].iter().collect();
    (visible, cursor - start)
}

fn draw_menu(f: &mut Frame, app: &App) {
    // Draw the top-of-stack level. Lower levels sit behind it; when this
    // one pops on Esc, the next tick redraws whichever was underneath.
    let Some(menu) = app.menu_stack.last() else { return };
    let labels = menu.labels();
    // Size the modal to the item count so a single-item Settings menu
    // doesn't look empty at the same height as a fuller menu.
    let height = (labels.len() as u16 + 4).max(5);
    let area = centered_rect(28, height, f.area());
    // Clear underneath so the menu isn't blended with what's below.
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(menu.title());

    let lines: Vec<Line<'static>> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let selected = i == menu.selected;
            let marker = if selected { "▶ " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(marker, style),
                Span::styled((*label).to_string(), style),
            ])
        })
        .collect();

    let para = Paragraph::new(lines)
        .style(Style::reset())
        .block(block);
    f.render_widget(para, area);
}

// Center a `width × height` rect inside `area`, clamped to fit.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_git_status(f: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(0)])
        .split(columns[0]);

    draw_heatmap(f, app, left[0]);
    draw_graph(f, app, left[1]);
    draw_right_pane(f, app, columns[1]);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Tabs;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::reset()
    };
    let highlight_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let titles: Vec<Line> = TAB_LABELS
        .iter()
        .map(|s| Line::from(Span::raw(*s)))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.selected_tab.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(highlight_style)
        .divider(Span::styled("│", Style::default().fg(Color::DarkGray)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style),
        );
    f.render_widget(tabs, area);
}

fn draw_placeholder(f: &mut Frame, _app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());
    let para = Paragraph::new("").block(block);
    f.render_widget(para, area);
}

fn draw_heatmap(f: &mut Frame, app: &App, area: Rect) {
    let base_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());

    let (title, lines) = match &app.git_tree {
        Some(tree) => {
            let (label, days) = match app.selected_repo.and_then(|i| tree.repos.get(i)) {
                Some(repo) => (repo.name.as_str(), &repo.commit_days),
                None => ("all repos", &tree.commit_days),
            };
            render_heatmap(label, days, area.width)
        }
        None => (
            " Commits ".to_string(),
            vec![Line::from(Span::styled(
                "scanning …",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    let para = Paragraph::new(lines)
        .style(Style::reset())
        .block(base_block.title(title));
    f.render_widget(para, area);
}

// Build the 7×N heatmap from a per-day commit count map. Today is in the
// rightmost (possibly partial) column; rows are Sun..Sat top-to-bottom.
fn render_heatmap(
    label: &str,
    commit_days: &HashMap<i64, u32>,
    area_width: u16,
) -> (String, Vec<Line<'static>>) {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today_day = now_secs / 86_400;
    // 1970-01-01 was a Thursday (=4 in Sun=0 scheme), so weekday = (day + 4) % 7.
    let today_dow = ((today_day + 4).rem_euclid(7)) as i64;

    // Reserve 4 chars for the weekday label column ("Mon ") and 2 for borders.
    // Each week column renders as 2 chars wide ("■ ") to keep cells from
    // looking stretched and to land on ~6 months across the panel.
    let inner = area_width.saturating_sub(2) as usize;
    let weeks = inner.saturating_sub(4) / 2;
    let weeks = weeks.min(HEATMAP_WEEKS_MAX);

    // grid[row][col] — None for cells outside the year window (e.g. days
    // after today in the current week).
    let mut grid: Vec<Vec<Option<u32>>> = vec![vec![None; weeks]; 7];
    let mut total: u32 = 0;
    for col in 0..weeks {
        let weeks_ago = (weeks - 1 - col) as i64;
        for row in 0..7i64 {
            let day = today_day - weeks_ago * 7 - (today_dow - row);
            if day > today_day {
                continue;
            }
            let count = commit_days.get(&day).copied().unwrap_or(0);
            grid[row as usize][col] = Some(count);
            total += count;
        }
    }

    let max_count = grid
        .iter()
        .flatten()
        .filter_map(|c| *c)
        .max()
        .unwrap_or(0);

    // Sunday's month per column — drives placement of month labels.
    let months: Vec<u32> = (0..weeks)
        .map(|col| {
            let weeks_ago = (weeks - 1 - col) as i64;
            let sunday = today_day - weeks_ago * 7 - today_dow;
            civil_from_days(sunday).1
        })
        .collect();

    // Per-day separators: separators[row][col] is true when the day in the
    // next column's same row belongs to a different month. Because the
    // boundary can fall mid-week, the line shifts between rows — a stepped
    // wiggle that traces the real month boundary instead of pretending every
    // month starts on Sunday.
    let mut separators: Vec<Vec<bool>> = vec![vec![false; weeks]; 7];
    for row in 0..7 {
        for col in 0..weeks.saturating_sub(1) {
            if grid[row][col].is_none() || grid[row][col + 1].is_none() {
                continue;
            }
            let weeks_ago_left = (weeks - 1 - col) as i64;
            let day_left = today_day - weeks_ago_left * 7 - (today_dow - row as i64);
            let day_right = day_left + 7;
            if civil_from_days(day_left).1 != civil_from_days(day_right).1 {
                separators[row][col] = true;
            }
        }
    }

    let day_labels = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(8);
    // The label row uses Sunday's separators since labels are placed at
    // each column whose Sunday opens a new month.
    lines.push(month_label_row(&months, &separators[0]));
    for row in 0..7 {
        // Show only Mon/Wed/Fri labels to mirror GitHub's compact layout.
        let label = if matches!(row, 1 | 3 | 5) {
            format!("{:>3} ", day_labels[row])
        } else {
            "    ".to_string()
        };
        let mut spans: Vec<Span<'static>> = vec![Span::styled(
            label,
            Style::default().fg(Color::DarkGray),
        )];
        for col in 0..weeks {
            spans.push(match grid[row][col] {
                Some(count) => Span::styled(
                    "■",
                    Style::default().fg(heatmap_color(count, max_count)),
                ),
                None => Span::raw(" "),
            });
            spans.push(if separators[row][col] {
                Span::styled("│", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw(" ")
            });
        }
        lines.push(Line::from(spans));
    }

    let title = format!(" Commits — {label} · {total} in view ");
    (title, lines)
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// Place a 3-char month label above the column where that month's first
// Sunday falls in the visible window. `next_free` guards against overlap.
fn month_label_row(months: &[u32], separators: &[bool]) -> Line<'static> {
    let weeks = months.len();
    let width = weeks * 2;
    let mut chars: Vec<char> = vec![' '; width];

    // Place month labels at each rollover. Cell positions take priority
    // over gap positions, so labels and separators never collide.
    let mut prev_month: u32 = 0;
    let mut next_free: usize = 0;
    for col in 0..weeks {
        let pos = col * 2;
        if months[col] != prev_month && pos >= next_free {
            let label = MONTH_NAMES[months[col] as usize - 1];
            for (i, ch) in label.chars().enumerate() {
                if pos + i < width {
                    chars[pos + i] = ch;
                }
            }
            next_free = pos + label.len() + 1;
        }
        prev_month = months[col];
    }

    let gray = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span<'static>> = vec![Span::raw("    ")];
    for col in 0..weeks {
        // Cell char (either a label letter or blank).
        spans.push(Span::styled(chars[col * 2].to_string(), gray));
        // Gap: label continuation wins, else separator, else blank.
        let gap = chars[col * 2 + 1];
        spans.push(if gap != ' ' {
            Span::styled(gap.to_string(), gray)
        } else if separators[col] {
            Span::styled("│", gray)
        } else {
            Span::raw(" ")
        });
    }
    Line::from(spans)
}

// Howard Hinnant's civil_from_days. Days are signed from 1970-01-01.
// Returns (year, month [1..=12], day [1..=31]).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn heatmap_color(count: u32, max: u32) -> Color {
    if count == 0 || max == 0 {
        return Color::Rgb(40, 44, 52);
    }
    let frac = count as f64 / max as f64;
    if frac > 0.66 {
        Color::Rgb(57, 211, 83)
    } else if frac > 0.33 {
        Color::Rgb(38, 166, 65)
    } else if frac > 0.10 {
        Color::Rgb(0, 109, 50)
    } else {
        Color::Rgb(14, 68, 41)
    }
}

fn draw_graph(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Left;
    let base_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::reset()
        })
        .style(Style::reset());

    let (title, lines): (String, Vec<Line<'static>>) = match (&app.git_tree, app.selected_repo) {
        (Some(tree), Some(idx)) if idx < tree.repos.len() => {
            let repo = &tree.repos[idx];
            let hint = if focused { " (↑/↓ scroll · Esc back) " } else { " (Esc back) " };
            let title = format!(" Graph — {}  {hint}", repo.name);
            let lines = match app.graph_cache.get(&repo.name) {
                Some(rows) => render_graph(rows),
                None => vec![Line::from(Span::styled(
                    "loading …",
                    Style::default().fg(Color::DarkGray),
                ))],
            };
            (title, lines)
        }
        (Some(tree), _) => {
            let hint = if focused { " (↑/↓ scroll) " } else { " " };
            let title = format!(
                " Recent commits — {} latest {hint}",
                tree.recent_commits.len(),
            );
            let lines = if tree.recent_commits.is_empty() {
                vec![Line::from(Span::styled(
                    "no commits found",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                render_recent_commits(&tree.recent_commits)
            };
            (title, lines)
        }
        (None, _) => (
            " Recent commits ".to_string(),
            vec![Line::from(Span::styled(
                "scanning …",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    // Cache the rendered size so Up/Down can clamp its scroll on the next tick.
    app.left_content_lines.set(lines.len() as u16);
    app.left_inner_height.set(area.height.saturating_sub(2));

    let para = Paragraph::new(lines)
        .style(Style::reset())
        .scroll((app.left_scroll, 0))
        .block(base_block.title(title));
    f.render_widget(para, area);
}

fn render_recent_commits(commits: &[RecentCommit]) -> Vec<Line<'static>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Pad repo names to the widest in view so subjects line up in a column.
    let repo_w = commits
        .iter()
        .map(|c| c.repo.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);

    commits
        .iter()
        .map(|c| {
            let age = format_age_short(now.saturating_sub(c.timestamp));
            let repo = truncate(&c.repo, repo_w);
            let pad = repo_w.saturating_sub(repo.chars().count());
            Line::from(vec![
                Span::styled(
                    format!("{age:>4}  "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(repo, Style::default().fg(Color::Cyan)),
                Span::raw(" ".repeat(pad + 2)),
                Span::raw(truncate(&c.subject, 80)),
            ])
        })
        .collect()
}

// Color the graph drawing column-by-column so each lane reads as a distinct
// color. This is an approximation — git's lanes can shift columns at merges
// and branches — but it's cheap and visually close to a true swim-lane view.
// Palette borrowed from VSCode's GitHub Graph / GitHub's own PR graph:
// blue leads (so `main` at column 0 gets the trunk color), then bright,
// well-separated hues that read clearly on both dark and light backgrounds.
const LANE_COLORS: [Color; 6] = [
    Color::Rgb(88, 166, 255),  // blue    — #58A6FF (trunk)
    Color::Rgb(247, 120, 186), // pink    — #F778BA
    Color::Rgb(126, 231, 135), // green   — #7EE787
    Color::Rgb(240, 184, 74),  // amber   — #F0B84A
    Color::Rgb(163, 113, 247), // purple  — #A371F7
    Color::Rgb(255, 122, 89),  // orange  — #FF7A59
];

fn lane_color(col: usize) -> Color {
    LANE_COLORS[(col / 2) % LANE_COLORS.len()]
}

// Diagonals sit in the gap columns between two lanes. In git's `--graph`
// output they always connect the lane on their right (the branch splitting
// off or merging in), so color them with that lane's hue rather than the
// trunk's. Without this the curves all read as blue.
fn glyph_color(col: usize, ch: char) -> Color {
    let effective_col = match ch {
        '/' | '\\' => col + 1,
        _ => col,
    };
    lane_color(effective_col)
}

// Turn one raw `git log --graph` glyph into its Unicode box-drawing sibling
// so lanes read as smooth vertical rails and merges as ring nodes, à la
// VSCode's GitHub Graph. `is_merge` only affects the commit glyph.
fn beautify_glyph(ch: char, is_merge: bool) -> char {
    match ch {
        '*' => {
            if is_merge {
                '◉'
            } else {
                '●'
            }
        }
        '|' => '│',
        '/' => '╱',
        '\\' => '╲',
        '_' => '─',
        c => c,
    }
}

fn render_graph(rows: &[GraphRow]) -> Vec<Line<'static>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    rows.iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            // Graph drawing chars — swapped for Unicode box-drawing glyphs
            // and colored by column so each lane reads as a distinct swim lane.
            for (i, ch) in row.prefix.chars().enumerate() {
                if ch == ' ' {
                    spans.push(Span::raw(" "));
                    continue;
                }
                let glyph = beautify_glyph(ch, row.is_merge);
                let mut style = Style::default().fg(glyph_color(i, ch));
                // Commit nodes are the visual anchors — bold makes them pop
                // above the lane rails without changing width.
                if ch == '*' {
                    style = style.add_modifier(Modifier::BOLD);
                }
                spans.push(Span::styled(glyph.to_string(), style));
            }

            // Connector-only rows (no commit on them) stop here.
            if row.sha.is_none() {
                return Line::from(spans);
            }

            // Refs as colored chips, à la the screenshot.
            for r in &row.refs {
                spans.push(Span::raw(" "));
                spans.push(ref_chip(r));
            }

            spans.push(Span::raw(" "));
            spans.push(Span::raw(row.subject.clone()));

            let age = row
                .timestamp
                .map(|t| format_age_short(now.saturating_sub(t)))
                .unwrap_or_default();
            spans.push(Span::styled(
                format!("  {age}"),
                Style::default().fg(Color::DarkGray),
            ));
            if !row.author.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", row.author),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            Line::from(spans)
        })
        .collect()
}

fn ref_chip(r: &str) -> Span<'static> {
    // VSCode-style: HEAD is a filled blue pill (the "you are here" marker),
    // tags get a filled amber pill, local branches get a bright blue-tinted
    // outline, and remote-tracking refs stay muted so the eye lands on HEAD.
    let (label, style) = if let Some(target) = r.strip_prefix("HEAD -> ") {
        (
            format!(" ◉ {target} "),
            Style::default()
                .bg(Color::Rgb(31, 111, 235)) // GitHub blue
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else if r == "HEAD" {
        (
            " ◉ HEAD ".to_string(),
            Style::default()
                .bg(Color::Rgb(31, 111, 235))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else if let Some(tag) = r.strip_prefix("tag: ") {
        (
            format!(" {tag} "),
            Style::default()
                .bg(Color::Rgb(219, 171, 10)) // amber
                .fg(Color::Black),
        )
    } else if r.contains('/') {
        // Remote-tracking branch — muted so HEAD/local refs stand out.
        (
            format!(" {r} "),
            Style::default().fg(Color::Rgb(139, 148, 158)),
        )
    } else {
        (
            format!(" {r} "),
            Style::default().fg(Color::Rgb(88, 166, 255)),
        )
    };
    Span::styled(label, style)
}

fn draw_right_pane(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Right;
    let base_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::reset()
        })
        .style(Style::reset());

    match &app.git_tree {
        Some(tree) => {
            // Round to 5-second increments so the title doesn't flicker every tick.
            let rounded = (tree.scanned_at.elapsed().as_secs() / 5) * 5;
            let hint = if focused { " (↑/↓ select) " } else { " " };
            let title = format!(
                " Git activity — {} repos in {}  (scanned {}s ago){hint}",
                tree.total_repos, tree.root_display, rounded,
            );
            let lines = render_git_tree(tree, app.selected_repo);
            let para = Paragraph::new(lines)
                .style(Style::reset())
                .block(base_block.title(title));
            f.render_widget(para, area);
        }
        None => {
            let para = Paragraph::new("scanning ~/Documents/code …")
                .style(Style::reset())
                .block(base_block.title(" Git activity "));
            f.render_widget(para, area);
        }
    }
}

struct TreeRow {
    spans: Vec<Span<'static>>,
    children: Vec<TreeRow>,
}

impl TreeRow {
    fn leaf(spans: Vec<Span<'static>>) -> Self {
        Self { spans, children: Vec::new() }
    }
    fn with_children(spans: Vec<Span<'static>>, children: Vec<TreeRow>) -> Self {
        Self { spans, children }
    }
}

fn render_tree_row(
    row: TreeRow,
    prefix: &str,
    is_last: bool,
    out: &mut Vec<Line<'static>>,
) {
    let connector = if is_last { "└─ " } else { "├─ " };
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        format!("{prefix}{connector}"),
        Style::default().fg(Color::DarkGray),
    )];
    spans.extend(row.spans);
    out.push(Line::from(spans));

    // Carry a vertical spine through children only if this row has siblings below.
    let continuation = if is_last { "  " } else { "│ " };
    let child_prefix = format!("{prefix}{continuation}");
    let last_child = row.children.len().saturating_sub(1);
    for (i, child) in row.children.into_iter().enumerate() {
        render_tree_row(child, &child_prefix, i == last_child, out);
    }
}

fn branch_spans(branch: &git::BranchInfo, now: u64) -> Vec<Span<'static>> {
    let branch_age = branch
        .last_commit
        .map(|t| format_age(now.saturating_sub(t)))
        .unwrap_or_default();
    let marker = if branch.is_current { "→" } else { " " };
    let track = match (branch.ahead, branch.behind) {
        (0, 0) => String::new(),
        (a, 0) => format!("  {a} ahead"),
        (0, b) => format!("  {b} behind"),
        (a, b) => format!("  {a} ahead, {b} behind"),
    };
    let active_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let (marker_style, name_style) = if branch.is_current {
        (active_style, active_style)
    } else {
        (Style::default().fg(Color::DarkGray), Style::default())
    };

    vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(branch.name.clone(), name_style),
        Span::styled(track, Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("   {branch_age}   "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            truncate(&branch.last_message, 40),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

// Pick the conventional trunk branch so feature branches nest under it.
// Prefers a local trunk; falls back to origin/main-style remote-only
// trunks so a repo that hasn't checked anything out still nests correctly.
fn pick_trunk_idx(branches: &[git::BranchInfo]) -> Option<usize> {
    const TRUNKS: [&str; 4] = ["main", "master", "develop", "trunk"];
    for name in TRUNKS {
        if let Some(i) = branches.iter().position(|b| b.name == name) {
            return Some(i);
        }
    }
    for name in TRUNKS {
        let remote = format!("origin/{name}");
        if let Some(i) = branches.iter().position(|b| b.name == remote) {
            return Some(i);
        }
    }
    None
}

fn render_git_tree(tree: &GitTree, selected: Option<usize>) -> Vec<Line<'static>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();

    for (idx, repo) in tree.repos.iter().enumerate() {
        let is_selected = selected == Some(idx);
        let age_secs = repo.most_recent_commit.map(|t| now.saturating_sub(t));
        let age_str = age_secs
            .map(format_age)
            .unwrap_or_else(|| "—".to_string());
        let (bullet, bullet_color) = match age_secs {
            Some(s) if s < 86_400 => ("●", Color::Green),
            Some(_) => ("○", Color::DarkGray),
            None => ("·", Color::DarkGray),
        };

        // Repo header — name, age, and (if applicable) the upstream pointer
        // all on one line. Selected repo gets a bright arrow + bold name.
        let marker = if is_selected { "▶" } else { " " };
        let marker_style = if is_selected {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let name_style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let mut header_spans = vec![
            Span::styled(marker.to_string(), marker_style),
            Span::raw(" "),
            Span::styled(bullet.to_string(), Style::default().fg(bullet_color)),
            Span::raw(" "),
            Span::styled(repo.name.clone(), name_style),
            Span::styled(
                format!("   {age_str}"),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(upstream) = &repo.upstream_remote {
            header_spans.push(Span::styled(
                "   forked from ".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
            header_spans.push(Span::styled(
                upstream.clone(),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(header_spans));

        let mut rows: Vec<TreeRow> = Vec::new();

        if repo.is_dirty {
            rows.push(TreeRow::leaf(vec![Span::styled(
                "uncommitted changes".to_string(),
                Style::default().fg(Color::Yellow),
            )]));
        }

        if let Some(d) = &repo.fork_drift {
            if d.ahead != 0 || d.behind != 0 {
                let text = match (d.ahead, d.behind) {
                    (a, 0) => format!("Fork {} is {} ahead of original", d.branch, a),
                    (0, b) => {
                        format!("Fork {} is {} behind original (pull from upstream)", d.branch, b)
                    }
                    (a, b) => format!(
                        "Fork {} has diverged: {} ahead, {} behind original",
                        d.branch, a, b
                    ),
                };
                rows.push(TreeRow::leaf(vec![Span::styled(
                    text,
                    Style::default().fg(Color::Yellow),
                )]));
            }
        }

        if repo.branches.is_empty() {
            rows.push(TreeRow::leaf(vec![Span::styled(
                "(no commits)".to_string(),
                Style::default().fg(Color::DarkGray),
            )]));
        } else {
            // Nest non-trunk branches under the trunk so feature branches read
            // as "branched off main" visually.
            match pick_trunk_idx(&repo.branches) {
                Some(idx) => {
                    let trunk = &repo.branches[idx];
                    let children: Vec<TreeRow> = repo
                        .branches
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != idx)
                        .map(|(_, b)| TreeRow::leaf(branch_spans(b, now)))
                        .collect();
                    rows.push(TreeRow::with_children(branch_spans(trunk, now), children));
                }
                None => {
                    for branch in &repo.branches {
                        rows.push(TreeRow::leaf(branch_spans(branch, now)));
                    }
                }
            }
        }

        // Indent 4 so the tree lines up under the repo name (the leading
        // "▶ ● " on the header is 4 cells).
        let last_idx = rows.len().saturating_sub(1);
        for (i, row) in rows.into_iter().enumerate() {
            render_tree_row(row, "    ", i == last_idx, &mut lines);
        }

        lines.push(Line::from(""));
    }

    lines
}

fn format_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 2_592_000 => format!("{}d ago", s / 86_400),
        s => format!("{}mo ago", s / 2_592_000),
    }
}

fn format_age_short(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 2_592_000 => format!("{}d", s / 86_400),
        s => format!("{}mo", s / 2_592_000),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

