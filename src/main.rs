mod config;
mod git;
mod hardware;
mod network;
mod processes;

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
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Tabs},
};

use config::Config;
use git::{GitTree, GraphRow, RecentCommit, Scanner, spawn_scanner};
use hardware::{BatterySnapshot, HardwareState};
use network::{AppStat, Monitor, NetworkState};
use processes::{ProcInfo, ProcessState};

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
    Processes,
    Network,
    DiskPower,
}

const TABS: [Tab; 4] = [Tab::GitStatus, Tab::Processes, Tab::Network, Tab::DiskPower];
const TAB_LABELS: [&str; 4] = ["Git Status", "Processes", "Network", "Disk / Power"];

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
    // Scroll offset for the right (git-activity) pane. Driven by selection:
    // moving the cursor past the visible bottom shifts this to bring the
    // selected repo back into view. Cell so draw() can nudge it without
    // needing &mut self.
    right_scroll: Cell<u16>,
    // Optional because `nettop` might fail to spawn (e.g. quarantined
    // binary, missing entitlement). We render a friendly error in that
    // case instead of crashing.
    net_monitor: Option<Monitor>,
    net_state: NetworkState,
    // Selection in the network app list, by canonical app name — a name
    // rather than an index so the highlight stays glued to its app when
    // the traffic-based sort reorders the list under the cursor.
    net_selected: Option<String>,
    // When Some, the network tab shows a full-screen detail view for
    // this app. Esc drops back to the list.
    net_detail: Option<String>,
    // Scroll offset for the network app list; nudged during draw to keep
    // the selection visible (same pattern as right_scroll).
    net_scroll: Cell<u16>,
    // Processes tab — same trio as the network tab: name-keyed selection,
    // full-screen detail, draw-nudged scroll offset.
    proc_monitor: processes::Monitor,
    proc_state: ProcessState,
    proc_selected: Option<String>,
    proc_detail: Option<String>,
    proc_scroll: Cell<u16>,
    // Disk/power tab — a pure dashboard, so state only, no selection.
    hw_monitor: hardware::Monitor,
    hw_state: HardwareState,
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
            right_scroll: Cell::new(0),
            net_monitor: network::spawn_monitor(),
            net_state: NetworkState::new(),
            net_selected: None,
            net_detail: None,
            net_scroll: Cell::new(0),
            proc_monitor: processes::spawn_monitor(),
            proc_state: ProcessState::new(),
            proc_selected: None,
            proc_detail: None,
            proc_scroll: Cell::new(0),
            hw_monitor: hardware::spawn_monitor(),
            hw_state: HardwareState::new(),
            menu_stack: Vec::new(),
            dir_prompt,
            should_quit: false,
        }
    }

    fn drain_network_updates(&mut self) {
        let Some(mon) = &self.net_monitor else { return };
        // Fold every pending sample. In practice one per tick, but
        // handles bursts (e.g. after a stall) cleanly.
        loop {
            match mon.try_recv() {
                Ok(sample) => self.net_state.apply_sample(sample),
                Err(_) => break,
            }
        }
        // An app pruned for inactivity can't stay selected or detailed.
        if let Some(n) = &self.net_detail {
            if !self.net_state.apps.contains_key(n) {
                self.net_detail = None;
            }
        }
        if let Some(n) = &self.net_selected {
            if !self.net_state.apps.contains_key(n) {
                self.net_selected = None;
            }
        }
    }

    fn drain_process_updates(&mut self) {
        loop {
            match self.proc_monitor.try_recv() {
                Ok(sample) => self.proc_state.apply_sample(sample),
                Err(_) => break,
            }
        }
        // An app whose processes all exited can't stay selected or detailed.
        if let Some(n) = &self.proc_detail {
            if !self.proc_state.apps.contains_key(n) {
                self.proc_detail = None;
            }
        }
        if let Some(n) = &self.proc_selected {
            if !self.proc_state.apps.contains_key(n) {
                self.proc_selected = None;
            }
        }
    }

    fn drain_hardware_updates(&mut self) {
        while let Ok(sample) = self.hw_monitor.try_recv() {
            self.hw_state.apply_sample(sample);
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

    fn net_sorted_names(&self) -> Vec<String> {
        self.net_state
            .sorted_apps()
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }

    // Move the network-list cursor by `delta` positions within the
    // current traffic-sorted order. With nothing selected, any move
    // lands on the top app.
    fn move_net_selection(&mut self, delta: i32) {
        let order = self.net_sorted_names();
        if order.is_empty() {
            return;
        }
        let next = match self
            .net_selected
            .as_ref()
            .and_then(|n| order.iter().position(|o| o == n))
        {
            None => 0,
            Some(i) => (i as i32 + delta).clamp(0, order.len() as i32 - 1) as usize,
        };
        self.net_selected = Some(order[next].clone());
    }

    // Returns true when the key was consumed by the network tab's own
    // interactions; false lets the shared handler (quit keys, tab-bar
    // cycling, menu) have it.
    fn handle_network_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false; // Ctrl-C etc. always reach the shared handler.
        }
        // Detail view: Esc drops back to the list; everything except
        // quit is inert while it's open.
        if self.net_detail.is_some() {
            return match key.code {
                KeyCode::Esc => {
                    self.net_detail = None;
                    true
                }
                KeyCode::Char('q') => false,
                _ => true,
            };
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Tabs {
                    // Drop from the tab bar into the list, cursor on top.
                    self.focus = Focus::Right;
                    if self.net_selected.is_none() {
                        self.move_net_selection(0);
                    }
                } else {
                    self.move_net_selection(1);
                }
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Tabs {
                    return true;
                }
                let order = self.net_sorted_names();
                let at_top = match &self.net_selected {
                    Some(n) => order.first() == Some(n),
                    None => true,
                };
                if at_top {
                    // Past the top of the list → back up to the tab bar.
                    self.focus = Focus::Tabs;
                    self.net_selected = None;
                } else {
                    self.move_net_selection(-1);
                }
                true
            }
            KeyCode::Enter => {
                if self.focus != Focus::Tabs {
                    if let Some(n) = &self.net_selected {
                        self.net_detail = Some(n.clone());
                    }
                }
                true
            }
            KeyCode::Esc => {
                if self.focus == Focus::Tabs {
                    false // shared handler opens the menu
                } else {
                    self.net_selected = None;
                    self.focus = Focus::Tabs;
                    true
                }
            }
            // In-list Left/Right is meaningless (single pane) — swallow
            // so the git-tab pane-switching semantics don't kick in.
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.focus != Focus::Tabs
            }
            _ => false,
        }
    }

    fn proc_sorted_names(&self) -> Vec<String> {
        self.proc_state
            .visible_apps()
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }

    // Move the process-list cursor by `delta` positions within the
    // current CPU-sorted order. Same semantics as move_net_selection.
    fn move_proc_selection(&mut self, delta: i32) {
        let order = self.proc_sorted_names();
        if order.is_empty() {
            return;
        }
        let next = match self
            .proc_selected
            .as_ref()
            .and_then(|n| order.iter().position(|o| o == n))
        {
            None => 0,
            Some(i) => (i as i32 + delta).clamp(0, order.len() as i32 - 1) as usize,
        };
        self.proc_selected = Some(order[next].clone());
    }

    // The processes tab's own interactions — a mirror of
    // handle_network_key with the proc_* state. Returns true when the key
    // was consumed; false lets the shared handler have it.
    fn handle_process_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false; // Ctrl-C etc. always reach the shared handler.
        }
        // Detail view: Esc drops back to the list; everything except
        // quit is inert while it's open.
        if self.proc_detail.is_some() {
            return match key.code {
                KeyCode::Esc => {
                    self.proc_detail = None;
                    true
                }
                KeyCode::Char('q') => false,
                _ => true,
            };
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Tabs {
                    // Drop from the tab bar into the list, cursor on top.
                    self.focus = Focus::Right;
                    if self.proc_selected.is_none() {
                        self.move_proc_selection(0);
                    }
                } else {
                    self.move_proc_selection(1);
                }
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Tabs {
                    return true;
                }
                let order = self.proc_sorted_names();
                let at_top = match &self.proc_selected {
                    Some(n) => order.first() == Some(n),
                    None => true,
                };
                if at_top {
                    // Past the top of the list → back up to the tab bar.
                    self.focus = Focus::Tabs;
                    self.proc_selected = None;
                } else {
                    self.move_proc_selection(-1);
                }
                true
            }
            KeyCode::Enter => {
                if self.focus != Focus::Tabs {
                    if let Some(n) = &self.proc_selected {
                        self.proc_detail = Some(n.clone());
                    }
                }
                true
            }
            KeyCode::Esc => {
                if self.focus == Focus::Tabs {
                    false // shared handler opens the menu
                } else {
                    self.proc_selected = None;
                    self.focus = Focus::Tabs;
                    true
                }
            }
            // In-list Left/Right is meaningless (single pane) — swallow
            // so the git-tab pane-switching semantics don't kick in.
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.focus != Focus::Tabs
            }
            _ => false,
        }
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
            self.right_scroll.set(0);
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
        // The network tab has its own selection/detail interactions; keys
        // it doesn't consume (quit, tab cycling, menu) fall through to
        // the shared handling below.
        if self.selected_tab == Tab::Network && self.handle_network_key(key) {
            return;
        }
        if self.selected_tab == Tab::Processes && self.handle_process_key(key) {
            return;
        }
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
                    self.right_scroll.set(0);
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
                // The disk/power tab is a dashboard with nothing to
                // select — there's no body to drop into.
                Focus::Tabs => {
                    if self.selected_tab != Tab::DiskPower {
                        self.enter_right_pane();
                    }
                }
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
        app.drain_network_updates();
        app.drain_process_updates();
        app.drain_hardware_updates();
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
        Tab::Processes => draw_processes(f, app, vertical[1]),
        Tab::Network => draw_network(f, app, vertical[1]),
        Tab::DiskPower => draw_disk_power(f, app, vertical[1]),
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
            let (lines, extents) = render_git_tree(tree, app.selected_repo);

            // Auto-scroll to keep the selected repo in view. The Paragraph
            // clips content past the pane's height, so without this a
            // selection past the bottom edge is invisible and Down feels
            // like it "stops" — which is exactly the bug this fixes.
            let inner_height = area.height.saturating_sub(2);
            let content_lines = lines.len() as u16;
            let max_scroll = content_lines.saturating_sub(inner_height);
            let mut scroll = app.right_scroll.get();
            if let Some(idx) = app.selected_repo {
                if let Some(&(start, height)) = extents.get(idx) {
                    if start < scroll {
                        // Selection is above viewport — reveal its top.
                        scroll = start;
                    } else if start + height > scroll + inner_height {
                        // Selection extends past viewport bottom.
                        // If the repo is taller than the pane we can't
                        // show all of it, so anchor to its top instead
                        // of hiding the header.
                        scroll = if height >= inner_height {
                            start
                        } else {
                            start + height - inner_height
                        };
                    }
                }
            }
            scroll = scroll.min(max_scroll);
            app.right_scroll.set(scroll);

            let para = Paragraph::new(lines)
                .style(Style::reset())
                .scroll((scroll, 0))
                .block(base_block.title(title));
            f.render_widget(para, area);
        }
        None => {
            let msg = format!("scanning {} …", config::display_path(&app.config.watch_dir));
            let para = Paragraph::new(msg)
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

// Returns the rendered lines and, for each repo, `(start_line, height)`
// covering that repo's block (header + tree rows + trailing blank).
// Callers use the extents to auto-scroll so the current selection stays
// visible without needing to re-walk the tree.
fn render_git_tree(
    tree: &GitTree,
    selected: Option<usize>,
) -> (Vec<Line<'static>>, Vec<(u16, u16)>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    let mut extents: Vec<(u16, u16)> = Vec::with_capacity(tree.repos.len());

    for (idx, repo) in tree.repos.iter().enumerate() {
        let start = lines.len() as u16;
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
        let height = lines.len() as u16 - start;
        extents.push((start, height));
    }

    (lines, extents)
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

// -------------------- network tab --------------------

// Fixed column widths so numbers align vertically across app cards.
const NET_NAME_COL: usize = 20;
// "↓ 999.9 KB/s  ↑ 999.9 KB/s" — two arrows + two right-aligned rates.
const NET_RATES_COL: usize = 26;
// Share-of-bandwidth bar width, in cells.
const NET_BAR_W: usize = 10;

// Unicode block heights for sparklines and charts, ordered low → high.
const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// Intensity ramps for the header charts (dim → bright), one per direction.
const DOWN_RAMP: [Color; 3] = [
    Color::Rgb(0, 109, 50),
    Color::Rgb(38, 166, 65),
    Color::Rgb(57, 211, 83),
];
const UP_RAMP: [Color; 3] = [
    Color::Rgb(16, 90, 110),
    Color::Rgb(34, 148, 172),
    Color::Rgb(86, 204, 242),
];

// Palette of accent colors assigned to apps by name hash, so each app
// keeps a stable, distinct hue across frames and runs.
const APP_ACCENTS: [Color; 7] = [
    Color::Rgb(88, 166, 255),  // blue
    Color::Rgb(247, 120, 186), // pink
    Color::Rgb(126, 231, 135), // green
    Color::Rgb(240, 184, 74),  // amber
    Color::Rgb(163, 113, 247), // purple
    Color::Rgb(255, 122, 89),  // orange
    Color::Rgb(118, 227, 234), // cyan
];

// FNV-1a over the app name → stable palette slot.
fn app_accent(name: &str) -> Color {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    APP_ACCENTS[h as usize % APP_ACCENTS.len()]
}

fn draw_network(f: &mut Frame, app: &App, area: Rect) {
    if app.net_monitor.is_none() {
        let msg = "network monitor unavailable — `nettop` failed to spawn.\n\
                   Try running `nettop -x -L 1` manually to confirm access.";
        let para = Paragraph::new(msg)
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title(" Network "));
        f.render_widget(para, area);
        return;
    }

    let state = &app.net_state;

    // Detail view replaces the whole tab body while open.
    if let Some(name) = &app.net_detail {
        if let Some(stat) = state.apps.get(name) {
            draw_app_detail(f, state, stat, area);
            return;
        }
    }

    // Header: side-by-side download/upload throughput charts (btop-style),
    // then the per-application list underneath.
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(zones[0]);

    draw_rate_panel(f, halves[0], '↓', state.ema_total_in, &state.history_in, &DOWN_RAMP);
    draw_rate_panel(f, halves[1], '↑', state.ema_total_out, &state.history_out, &UP_RAMP);
    draw_app_list(f, app, zones[1]);
}

// One direction's throughput panel: current smoothed rate + window peak in
// the title, rolling history as an intensity-colored bar chart inside.
fn draw_rate_panel(
    f: &mut Frame,
    area: Rect,
    arrow: char,
    ema_bps: f64,
    history: &std::collections::VecDeque<u32>,
    ramp: &[Color; 3],
) {
    let peak = history.iter().copied().max().unwrap_or(0) as f64;
    let title = Line::from(vec![
        Span::styled(
            format!(" {arrow} {} ", format_bps(ema_bps)),
            Style::default().fg(ramp[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {} ", format_bps(peak)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    draw_chart_block(f, area, title, history, ramp, None);
}

// Bordered block with `title`, filled with an intensity-colored history
// chart sized to whatever space the block encloses. `fixed_max` pins the
// chart's ceiling to an absolute scale (e.g. 100% CPU, total RAM); None
// normalizes against the window peak, right for unbounded rates.
fn draw_chart_block(
    f: &mut Frame,
    area: Rect,
    title: Line<'static>,
    history: &std::collections::VecDeque<u32>,
    ramp: &[Color; 3],
    fixed_max: Option<u32>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = render_history_bars(
        history,
        inner.width as usize,
        inner.height as usize,
        ramp,
        fixed_max,
    );
    f.render_widget(Paragraph::new(lines).style(Style::reset()), inner);
}

// Full-tab detail view for one app: tall download/upload history charts
// (8 minutes of samples) and the complete remote-host table.
fn draw_app_detail(f: &mut Frame, state: &NetworkState, stat: &AppStat, area: Rect) {
    let accent = app_accent(&stat.name);
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(0),
        ])
        .split(area);

    let peak_in = stat.history_in.iter().copied().max().unwrap_or(0) as f64;
    let down_title = Line::from(vec![
        Span::styled(
            format!(" ● {} ", stat.name),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· ↓ {} ", format_bps(stat.ema_bps_in)),
            Style::default().fg(DOWN_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {} ", format_bps(peak_in)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    draw_chart_block(f, zones[0], down_title, &stat.history_in, &DOWN_RAMP, None);

    let peak_out = stat.history_out.iter().copied().max().unwrap_or(0) as f64;
    let up_title = Line::from(vec![
        Span::styled(
            format!(" ↑ {} ", format_bps(stat.ema_bps_out)),
            Style::default().fg(UP_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {} ", format_bps(peak_out)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    draw_chart_block(f, zones[1], up_title, &stat.history_out, &UP_RAMP, None);

    draw_detail_hosts(f, state, stat, zones[2]);
}

// Every remote host the app is talking to, busiest first: activity
// bullet, hostname, share-of-app-traffic bar, conns, and rates.
fn draw_detail_hosts(f: &mut Frame, state: &NetworkState, stat: &AppStat, area: Rect) {
    let accent = app_accent(&stat.name);
    let hosts = stat.top_hosts(stat.hosts.len());
    let conns = if stat.conn_count == 0 {
        "idle".to_string()
    } else {
        format!(
            "{} conn{}",
            stat.conn_count,
            if stat.conn_count == 1 { "" } else { "s" },
        )
    };
    let title = format!(
        " Remote hosts — {} host{} · {conns} · Esc to close ",
        hosts.len(),
        if hosts.len() == 1 { "" } else { "s" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner_width = area.width.saturating_sub(2) as usize;

    // 4 indent+bullet · name · 1 · bar 10 · pct 5 · 2 · conns 11 · 2 ·
    // rates · 2 margin — same right-edge alignment as the list view.
    let name_w = inner_width
        .saturating_sub(4 + 1 + NET_BAR_W + 5 + 2 + 11 + 2 + NET_RATES_COL + 2)
        .max(10);
    let app_total = stat.total_ema_bps();

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    if hosts.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no remote hosts in the last 2 minutes.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (ip, host) in hosts {
        let idle_secs = host.last_active.elapsed().as_secs();
        let dim = idle_secs >= 30;
        let bullet = if idle_secs < 4 {
            "●"
        } else if idle_secs < 30 {
            "○"
        } else {
            "·"
        };
        let bullet_style = if dim {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(accent)
        };
        let name = state.hostname_for(&ip).unwrap_or_else(|| ip.to_string());
        let name_display = truncate(&name, name_w);
        let name_pad = name_w.saturating_sub(name_display.chars().count());
        let name_style = if dim {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };

        let share = if app_total > 0.0 {
            host.total_ema_bps() / app_total
        } else {
            0.0
        };
        let filled = ((share * NET_BAR_W as f64).round() as usize).min(NET_BAR_W);
        let bar_color = if dim { Color::DarkGray } else { accent };
        let conns = if host.conn_count == 0 {
            "idle".to_string()
        } else {
            format!(
                "{} conn{}",
                host.conn_count,
                if host.conn_count == 1 { "" } else { "s" },
            )
        };

        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(bullet.to_string(), bullet_style),
            Span::raw(" "),
            Span::styled(name_display, name_style),
            Span::raw(" ".repeat(name_pad + 1)),
            Span::styled("▰".repeat(filled), Style::default().fg(bar_color)),
            Span::styled(
                "▱".repeat(NET_BAR_W - filled),
                Style::default().fg(Color::Rgb(60, 66, 74)),
            ),
            Span::styled(
                format!(" {:>3.0}%", share * 100.0),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(format!("{conns:>11}"), Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
        ];
        spans.extend(rate_spans(host.ema_bps_in, host.ema_bps_out, dim));
        lines.push(Line::from(spans));
    }

    let para = Paragraph::new(lines).style(Style::reset()).block(block);
    f.render_widget(para, area);
}

// Render a rate history as a multi-row bar chart built from stacked
// eighth-blocks, colored by intensity. Newest sample at the right edge;
// short histories are left-padded so the chart grows in from the right.
fn render_history_bars(
    history: &std::collections::VecDeque<u32>,
    width: usize,
    rows: usize,
    ramp: &[Color; 3],
    fixed_max: Option<u32>,
) -> Vec<Line<'static>> {
    let pad = width.saturating_sub(history.len());
    let skip = history.len().saturating_sub(width);
    let vals: Vec<u32> = history.iter().skip(skip).copied().collect();
    let max = fixed_max
        .unwrap_or_else(|| vals.iter().copied().max().unwrap_or(0))
        .max(1);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
    for row in (0..rows).rev() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(width + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        for &v in &vals {
            let ratio = v as f64 / max as f64;
            // Column height in eighths of a row; non-zero traffic always
            // gets at least one eighth so trickles stay visible.
            let eighths = if v == 0 {
                0
            } else {
                ((ratio * (rows * 8) as f64).round() as usize).max(1)
            };
            let cell = eighths.saturating_sub(row * 8).min(8);
            if cell == 0 {
                spans.push(Span::raw(" "));
                continue;
            }
            let color = if ratio > 0.66 {
                ramp[2]
            } else if ratio > 0.33 {
                ramp[1]
            } else {
                ramp[0]
            };
            spans.push(Span::styled(
                SPARK_CHARS[cell - 1].to_string(),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

// Keep the selected card in view — Paragraph clips past the block height,
// so without this the cursor could walk below the visible bottom and
// appear stuck. Nudges the Cell-held offset during draw (same pattern as
// right_scroll) and returns the clamped value to render with.
fn scroll_into_view(
    cell: &Cell<u16>,
    selected_extent: Option<(u16, u16)>,
    total_lines: u16,
    inner_height: u16,
) -> u16 {
    let max_scroll = total_lines.saturating_sub(inner_height);
    let mut scroll = cell.get();
    if let Some((start, height)) = selected_extent {
        if start < scroll {
            scroll = start;
        } else if start + height > scroll + inner_height {
            scroll = if height >= inner_height {
                start
            } else {
                start + height - inner_height
            };
        }
    }
    scroll = scroll.min(max_scroll);
    cell.set(scroll);
    scroll
}

fn draw_app_list(f: &mut Frame, app: &App, area: Rect) {
    let state = &app.net_state;
    let apps = state.sorted_apps();
    let focused = app.focus == Focus::Right;
    let sampled = state
        .last_sample_at
        .map(|t| format!("sampled {}s ago", (t.elapsed().as_secs() / 2) * 2))
        .unwrap_or_else(|| "waiting for first sample".to_string());
    let hint = if focused {
        " (↑/↓ select · Enter details)"
    } else {
        ""
    };
    let title = format!(
        " Network — {} app{} · {sampled}{hint} ",
        apps.len(),
        if apps.len() == 1 { "" } else { "s" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2);

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    // (start_line, height) of the selected card, for scroll-into-view.
    let mut selected_extent: Option<(u16, u16)> = None;
    if state.last_sample_at.is_none() {
        lines.push(Line::from(Span::styled(
            "  gathering first samples …",
            Style::default().fg(Color::DarkGray),
        )));
    } else if apps.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no external traffic in the last 2 minutes.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Shares are relative to the sum over apps (not the raw system
        // total) so the bars always add up to a full 100%.
        let total_bps: f64 = apps.iter().map(|a| a.total_ema_bps()).sum();
        for stat in apps {
            let selected =
                focused && app.net_selected.as_deref() == Some(stat.name.as_str());
            let start = lines.len() as u16;
            append_app_card(&mut lines, stat, state, inner_width, total_bps, selected);
            lines.push(Line::from(""));
            if selected {
                selected_extent = Some((start, lines.len() as u16 - start));
            }
        }
    }

    let scroll = scroll_into_view(&app.net_scroll, selected_extent, lines.len() as u16, inner_height);

    let para = Paragraph::new(lines)
        .style(Style::reset())
        .scroll((scroll, 0))
        .block(block);
    f.render_widget(para, area);
}

// One application "card": headline row (accent-colored name · share bar ·
// sparkline · rates) plus its busiest remote hosts nested underneath.
fn append_app_card(
    lines: &mut Vec<Line<'static>>,
    stat: &AppStat,
    state: &NetworkState,
    inner_width: usize,
    total_bps: f64,
    selected: bool,
) {
    let accent = app_accent(&stat.name);
    let idle_secs = stat.last_active.elapsed().as_secs();
    let dim = idle_secs >= 30;
    let bullet = if idle_secs < 4 {
        "●"
    } else if idle_secs < 30 {
        "○"
    } else {
        "·"
    };
    let bullet_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent)
    };

    let name_display = truncate(&stat.name, NET_NAME_COL);
    let name_pad = NET_NAME_COL.saturating_sub(name_display.chars().count());
    let name_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    };

    // Drop the share bar on narrow terminals rather than squeezing the
    // sparkline out of existence.
    let show_bar = inner_width >= 82;
    let bar_cells = if show_bar { NET_BAR_W + 6 } else { 0 };
    // Sparkline soaks up all remaining width so the rate columns land on
    // the right edge — the same edge the host rows below align to.
    let fixed = 4 + NET_NAME_COL + 1 + bar_cells + 2 + NET_RATES_COL + 2;
    let spark_w = inner_width.saturating_sub(fixed).max(6);

    // Selected card gets the same "▶" cursor the git pane uses.
    let marker: Span<'static> = if selected {
        Span::styled("▶ ", Style::default().fg(Color::Cyan))
    } else {
        Span::raw("  ")
    };
    let mut spans: Vec<Span<'static>> = vec![
        marker,
        Span::styled(bullet.to_string(), bullet_style),
        Span::raw(" "),
        Span::styled(name_display, name_style),
        Span::raw(" ".repeat(name_pad + 1)),
    ];

    if show_bar {
        let share = if total_bps > 0.0 {
            stat.total_ema_bps() / total_bps
        } else {
            0.0
        };
        let filled = ((share * NET_BAR_W as f64).round() as usize).min(NET_BAR_W);
        let bar_color = if dim { Color::DarkGray } else { accent };
        spans.push(Span::styled(
            "▰".repeat(filled),
            Style::default().fg(bar_color),
        ));
        spans.push(Span::styled(
            "▱".repeat(NET_BAR_W - filled),
            Style::default().fg(Color::Rgb(60, 66, 74)),
        ));
        spans.push(Span::styled(
            format!(" {:>3.0}%", share * 100.0),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
    }

    let spark_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent)
    };
    spans.push(Span::styled(
        build_sparkline(&stat.history, spark_w),
        spark_style,
    ));
    spans.push(Span::raw("  "));
    spans.extend(rate_spans(stat.ema_bps_in, stat.ema_bps_out, dim));
    lines.push(Line::from(spans));

    // Busiest remote hosts, nested as a dim tree so the card reads
    // "app → who it's talking to" at a glance.
    let top = stat.top_hosts(3);
    let extra_hosts = stat.hosts.len().saturating_sub(top.len());
    // Sized so the host rows' rate columns land on the same right edge as
    // the app row above: 4 indent + 3 connector + name + 1 + 11 conns +
    // 2 + rates + 2 margin.
    let host_name_w = inner_width
        .saturating_sub(4 + 3 + 1 + 11 + 2 + NET_RATES_COL + 2)
        .max(10);
    let tree_style = Style::default().fg(Color::DarkGray);
    for (i, (ip, host)) in top.iter().enumerate() {
        let is_last = i == top.len() - 1 && extra_hosts == 0;
        let connector = if is_last { "└─ " } else { "├─ " };
        let name = state.hostname_for(ip).unwrap_or_else(|| ip.to_string());
        let host_display = truncate(&name, host_name_w);
        let host_pad = host_name_w.saturating_sub(host_display.chars().count());
        let host_dim = dim || host.last_active.elapsed().as_secs() >= 30;
        // conn_count decays to 0 when the connection closes — read that
        // as "idle" rather than the nonsensical "0 conns".
        let conns = if host.conn_count == 0 {
            "idle".to_string()
        } else {
            format!(
                "{} conn{}",
                host.conn_count,
                if host.conn_count == 1 { "" } else { "s" },
            )
        };
        let mut host_spans: Vec<Span<'static>> = vec![
            Span::raw("    "),
            Span::styled(connector.to_string(), tree_style),
            Span::styled(host_display, Style::default().fg(Color::Gray)),
            Span::raw(" ".repeat(host_pad + 1)),
            Span::styled(format!("{conns:>11}"), tree_style),
            Span::raw("  "),
        ];
        host_spans.extend(rate_spans(host.ema_bps_in, host.ema_bps_out, host_dim));
        lines.push(Line::from(host_spans));
    }
    if extra_hosts > 0 {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!(
                    "└─ … {extra_hosts} more host{}",
                    if extra_hosts == 1 { "" } else { "s" },
                ),
                tree_style,
            ),
        ]));
    }
}

// "↓ 1.2 MB/s  ↑ 40 KB/s" as styled spans, right-aligned into fixed
// columns so every row's numbers line up vertically.
fn rate_spans(bps_in: f64, bps_out: f64, dim: bool) -> Vec<Span<'static>> {
    let (in_style, out_style) = if dim {
        (
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            Style::default().fg(Color::Green),
            Style::default().fg(Color::Cyan),
        )
    };
    vec![
        Span::styled(format!("↓ {:>10}", format_bps(bps_in)), in_style),
        Span::raw("  "),
        Span::styled(format!("↑ {:>10}", format_bps(bps_out)), out_style),
    ]
}

// bytes/sec → human-readable with an SI (decimal) suffix. Decimal because
// network throughput is conventionally reported that way and it lines up
// with what other tools (nettop, iftop, bandwhich) show.
fn format_bps(bps: f64) -> String {
    let bps = bps.max(0.0);
    if bps < 1000.0 {
        format!("{:.0} B/s", bps)
    } else if bps < 1_000_000.0 {
        format!("{:.1} KB/s", bps / 1000.0)
    } else if bps < 1_000_000_000.0 {
        format!("{:.2} MB/s", bps / 1_000_000.0)
    } else {
        format!("{:.2} GB/s", bps / 1_000_000_000.0)
    }
}

// Render the rolling history as a fixed-width unicode block sparkline.
// Height per column is normalised against the app's own peak so the
// shape shows *burstiness* rather than raw amplitude — absolute magnitude
// is conveyed by the numeric rate on the same line.
fn build_sparkline(history: &std::collections::VecDeque<u32>, width: usize) -> String {
    if width == 0 { return String::new(); }
    let max = history.iter().copied().max().unwrap_or(0);
    let mut out = String::with_capacity(width * 3); // block chars are multi-byte
    // Left-pad with spaces if we don't have `width` samples yet, so
    // freshly-added apps don't have their sparkline hug the name column.
    if history.len() < width {
        for _ in 0..(width - history.len()) {
            out.push(' ');
        }
    }
    let skip = history.len().saturating_sub(width);
    for &v in history.iter().skip(skip) {
        if v == 0 {
            // Idle samples render as blanks so bursts stand out.
            out.push(' ');
        } else if max == 0 {
            out.push(SPARK_CHARS[0]);
        } else {
            let ratio = v as f64 / max as f64;
            let idx = (ratio * 7.0).round().clamp(0.0, 7.0) as usize;
            out.push(SPARK_CHARS[idx]);
        }
    }
    out
}

// -------------------- processes tab --------------------

// Fixed column widths, same role as the NET_* counterparts.
const PROC_NAME_COL: usize = 20;
// "cpu 999.9%  mem 63.9 GB" — two labels + two right-aligned values.
const PROC_STATS_COL: usize = 23;

// Intensity ramps for the header charts (dim → bright): amber for CPU,
// violet for memory — distinct from the network tab's green/cyan.
const CPU_RAMP: [Color; 3] = [
    Color::Rgb(133, 77, 14),
    Color::Rgb(202, 138, 4),
    Color::Rgb(250, 204, 21),
];
const MEM_RAMP: [Color; 3] = [
    Color::Rgb(76, 29, 149),
    Color::Rgb(124, 58, 237),
    Color::Rgb(167, 139, 250),
];

// bytes → human-readable, binary divisors (matching how Activity Monitor
// and htop report memory, unlike the decimal convention for throughput).
fn format_mem(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K * K {
        format!("{:.0} KB", b / K)
    } else if b < K * K * K {
        format!("{:.0} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

// "cpu 12.3%  mem 1.2 GB" as styled spans, right-aligned into fixed
// columns so every row's numbers line up vertically.
fn proc_stat_spans(cpu: f64, mem: u64, dim: bool) -> Vec<Span<'static>> {
    let (cpu_style, mem_style) = if dim {
        (
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            Style::default().fg(CPU_RAMP[2]),
            Style::default().fg(MEM_RAMP[2]),
        )
    };
    vec![
        Span::styled(format!("cpu {:>6}", format!("{cpu:.1}%")), cpu_style),
        Span::raw("  "),
        Span::styled(format!("mem {:>7}", format_mem(mem)), mem_style),
    ]
}

fn draw_processes(f: &mut Frame, app: &App, area: Rect) {
    let state = &app.proc_state;

    // Detail view replaces the whole tab body while open.
    if let Some(name) = &app.proc_detail {
        if let Some(stat) = state.apps.get(name) {
            draw_proc_detail(f, stat, area);
            return;
        }
    }

    // Header: side-by-side CPU/memory charts, then the per-application
    // list underneath — the same skeleton as the network tab.
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(zones[0]);

    let cpu_title = Line::from(vec![
        Span::styled(
            format!(" cpu {:.1}% ", state.ema_cpu_total),
            Style::default().fg(CPU_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} cores · load {:.2} ", state.core_count, state.load_avg),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    // Pinned to a 0–100% scale so the chart height means absolute load,
    // unlike the peak-normalized network charts.
    draw_chart_block(f, halves[0], cpu_title, &state.history_cpu, &CPU_RAMP, Some(1000));

    let mem_title = Line::from(vec![
        Span::styled(
            format!(" mem {} ", format_mem(state.mem_used)),
            Style::default().fg(MEM_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("of {} ", format_mem(state.mem_total)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let mem_max = (state.mem_total / (1024 * 1024)).max(1) as u32;
    draw_chart_block(f, halves[1], mem_title, &state.history_mem, &MEM_RAMP, Some(mem_max));

    draw_proc_list(f, app, zones[1]);
}

fn draw_proc_list(f: &mut Frame, app: &App, area: Rect) {
    let state = &app.proc_state;
    let apps = state.visible_apps();
    let focused = app.focus == Focus::Right;
    let sampled = state
        .last_sample_at
        .map(|t| format!("sampled {}s ago", (t.elapsed().as_secs() / 2) * 2))
        .unwrap_or_else(|| "waiting for first sample".to_string());
    let hint = if focused {
        " (↑/↓ select · Enter details)"
    } else {
        ""
    };
    let title = format!(
        " Processes — {} of {} apps · {} procs · {sampled}{hint} ",
        apps.len(),
        state.apps.len(),
        state.total_proc_count(),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2);

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    // (start_line, height) of the selected card, for scroll-into-view.
    let mut selected_extent: Option<(u16, u16)> = None;
    if state.last_sample_at.is_none() {
        lines.push(Line::from(Span::styled(
            "  gathering first samples …",
            Style::default().fg(Color::DarkGray),
        )));
    } else if apps.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing above the activity threshold.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Shares are relative to the sum over listed apps so the bars
        // always add up to a full 100%.
        let total_cpu: f64 = apps.iter().map(|a| a.ema_cpu).sum();
        for stat in apps {
            let selected =
                focused && app.proc_selected.as_deref() == Some(stat.name.as_str());
            let start = lines.len() as u16;
            append_proc_card(&mut lines, stat, inner_width, total_cpu, selected);
            lines.push(Line::from(""));
            if selected {
                selected_extent = Some((start, lines.len() as u16 - start));
            }
        }
    }

    let scroll = scroll_into_view(&app.proc_scroll, selected_extent, lines.len() as u16, inner_height);

    let para = Paragraph::new(lines)
        .style(Style::reset())
        .scroll((scroll, 0))
        .block(block);
    f.render_widget(para, area);
}

// One application "card": headline row (accent-colored name · share-of-CPU
// bar · sparkline · cpu/mem) plus its busiest member processes nested
// underneath — the processes-tab analog of append_app_card.
fn append_proc_card(
    lines: &mut Vec<Line<'static>>,
    stat: &processes::AppStat,
    inner_width: usize,
    total_cpu: f64,
    selected: bool,
) {
    let accent = app_accent(&stat.name);
    let idle_secs = stat.last_active.elapsed().as_secs();
    let dim = idle_secs >= 30;
    let bullet = if idle_secs < 4 {
        "●"
    } else if idle_secs < 30 {
        "○"
    } else {
        "·"
    };
    let bullet_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent)
    };

    let name_display = truncate(&stat.name, PROC_NAME_COL);
    let name_pad = PROC_NAME_COL.saturating_sub(name_display.chars().count());
    let name_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    };

    // Drop the share bar on narrow terminals rather than squeezing the
    // sparkline out of existence.
    let show_bar = inner_width >= 82;
    let bar_cells = if show_bar { NET_BAR_W + 6 } else { 0 };
    let fixed = 4 + PROC_NAME_COL + 1 + bar_cells + 2 + PROC_STATS_COL + 2;
    let spark_w = inner_width.saturating_sub(fixed).max(6);

    let marker: Span<'static> = if selected {
        Span::styled("▶ ", Style::default().fg(Color::Cyan))
    } else {
        Span::raw("  ")
    };
    let mut spans: Vec<Span<'static>> = vec![
        marker,
        Span::styled(bullet.to_string(), bullet_style),
        Span::raw(" "),
        Span::styled(name_display, name_style),
        Span::raw(" ".repeat(name_pad + 1)),
    ];

    if show_bar {
        let share = if total_cpu > 0.0 {
            stat.ema_cpu / total_cpu
        } else {
            0.0
        };
        let filled = ((share * NET_BAR_W as f64).round() as usize).min(NET_BAR_W);
        let bar_color = if dim { Color::DarkGray } else { accent };
        spans.push(Span::styled(
            "▰".repeat(filled),
            Style::default().fg(bar_color),
        ));
        spans.push(Span::styled(
            "▱".repeat(NET_BAR_W - filled),
            Style::default().fg(Color::Rgb(60, 66, 74)),
        ));
        spans.push(Span::styled(
            format!(" {:>3.0}%", share * 100.0),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
    }

    let spark_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(accent)
    };
    spans.push(Span::styled(
        build_sparkline(&stat.history, spark_w),
        spark_style,
    ));
    spans.push(Span::raw("  "));
    spans.extend(proc_stat_spans(stat.ema_cpu, stat.mem, dim));
    lines.push(Line::from(spans));

    // Busiest member processes, nested as a dim tree so the card reads
    // "app → what it's made of" at a glance.
    let top: Vec<&ProcInfo> = stat.procs.iter().take(3).collect();
    let extra = stat.procs.len().saturating_sub(top.len());
    // Sized so the member rows' stat columns land on the same right edge
    // as the app row above: 4 indent + 3 connector + name + 1 + 11 pid +
    // 2 + stats + 2 margin.
    let member_name_w = inner_width
        .saturating_sub(4 + 3 + 1 + 11 + 2 + PROC_STATS_COL + 2)
        .max(10);
    let tree_style = Style::default().fg(Color::DarkGray);
    for (i, p) in top.iter().enumerate() {
        let is_last = i == top.len() - 1 && extra == 0;
        let connector = if is_last { "└─ " } else { "├─ " };
        let member_display = truncate(&p.name, member_name_w);
        let member_pad = member_name_w.saturating_sub(member_display.chars().count());
        let member_dim = dim || p.cpu < 0.1;
        let mut member_spans: Vec<Span<'static>> = vec![
            Span::raw("    "),
            Span::styled(connector.to_string(), tree_style),
            Span::styled(member_display, Style::default().fg(Color::Gray)),
            Span::raw(" ".repeat(member_pad + 1)),
            Span::styled(
                format!("{:>11}", format!("pid {}", p.pid)),
                tree_style,
            ),
            Span::raw("  "),
        ];
        member_spans.extend(proc_stat_spans(p.cpu as f64, p.mem, member_dim));
        lines.push(Line::from(member_spans));
    }
    if extra > 0 {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!(
                    "└─ … {extra} more process{}",
                    if extra == 1 { "" } else { "es" },
                ),
                tree_style,
            ),
        ]));
    }
}

// Full-tab detail view for one app: tall CPU/memory history charts
// (8 minutes of samples) and the complete member-process table.
fn draw_proc_detail(f: &mut Frame, stat: &processes::AppStat, area: Rect) {
    let accent = app_accent(&stat.name);
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(0),
        ])
        .split(area);

    let peak_cpu = stat.history_cpu.iter().copied().max().unwrap_or(0) as f64 / 10.0;
    let cpu_title = Line::from(vec![
        Span::styled(
            format!(" ● {} ", stat.name),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· cpu {:.1}% ", stat.ema_cpu),
            Style::default().fg(CPU_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {peak_cpu:.1}% "),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    draw_chart_block(f, zones[0], cpu_title, &stat.history_cpu, &CPU_RAMP, None);

    let peak_mem = stat.history_mem.iter().copied().max().unwrap_or(0) as u64 * 1024 * 1024;
    let mem_title = Line::from(vec![
        Span::styled(
            format!(" mem {} ", format_mem(stat.mem)),
            Style::default().fg(MEM_RAMP[2]).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {} ", format_mem(peak_mem)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    draw_chart_block(f, zones[1], mem_title, &stat.history_mem, &MEM_RAMP, None);

    draw_detail_procs(f, stat, zones[2]);
}

// Every member process of the app, busiest first: activity bullet, name,
// share-of-app-CPU bar, pid, and cpu/mem — the processes-tab analog of
// draw_detail_hosts.
fn draw_detail_procs(f: &mut Frame, stat: &processes::AppStat, area: Rect) {
    let accent = app_accent(&stat.name);
    let procs = &stat.procs;
    let title = format!(
        " Processes — {} proc{} · Esc to close ",
        procs.len(),
        if procs.len() == 1 { "" } else { "s" },
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner_width = area.width.saturating_sub(2) as usize;

    // 4 indent+bullet · name · 1 · bar 10 · pct 5 · 2 · pid 11 · 2 ·
    // stats · 2 margin — same right-edge alignment as the list view.
    let name_w = inner_width
        .saturating_sub(4 + 1 + NET_BAR_W + 5 + 2 + 11 + 2 + PROC_STATS_COL + 2)
        .max(10);
    let app_cpu: f64 = procs.iter().map(|p| p.cpu as f64).sum();

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    for p in procs {
        let dim = p.cpu < 0.1;
        let bullet = if p.cpu >= 0.5 {
            "●"
        } else if p.cpu > 0.0 {
            "○"
        } else {
            "·"
        };
        let bullet_style = if dim {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(accent)
        };
        let name_display = truncate(&p.name, name_w);
        let name_pad = name_w.saturating_sub(name_display.chars().count());
        let name_style = if dim {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };

        let share = if app_cpu > 0.0 {
            p.cpu as f64 / app_cpu
        } else {
            0.0
        };
        let filled = ((share * NET_BAR_W as f64).round() as usize).min(NET_BAR_W);
        let bar_color = if dim { Color::DarkGray } else { accent };

        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(bullet.to_string(), bullet_style),
            Span::raw(" "),
            Span::styled(name_display, name_style),
            Span::raw(" ".repeat(name_pad + 1)),
            Span::styled("▰".repeat(filled), Style::default().fg(bar_color)),
            Span::styled(
                "▱".repeat(NET_BAR_W - filled),
                Style::default().fg(Color::Rgb(60, 66, 74)),
            ),
            Span::styled(
                format!(" {:>3.0}%", share * 100.0),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{:>11}", format!("pid {}", p.pid)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
        ];
        spans.extend(proc_stat_spans(p.cpu as f64, p.mem, dim));
        lines.push(Line::from(spans));
    }

    let para = Paragraph::new(lines).style(Style::reset()).block(block);
    f.render_widget(para, area);
}

// -------------------- disk / power tab --------------------

// Intensity ramps (dim → bright): blue for disk reads, magenta for
// writes. Battery flow reuses DOWN_RAMP (green) for charging and
// CPU_RAMP (amber) for draining.
const READ_RAMP: [Color; 3] = [
    Color::Rgb(30, 64, 175),
    Color::Rgb(59, 130, 246),
    Color::Rgb(147, 197, 253),
];
const WRITE_RAMP: [Color; 3] = [
    Color::Rgb(157, 23, 77),
    Color::Rgb(219, 39, 119),
    Color::Rgb(244, 114, 182),
];

// Volume fill bar width — wider than the share bars because it's the
// row's centerpiece rather than an annotation.
const VOL_BAR_W: usize = 24;
// Charge/health gauge width in the battery panel.
const PWR_GAUGE_W: usize = 16;

fn draw_disk_power(f: &mut Frame, app: &App, area: Rect) {
    let state = &app.hw_state;

    // Volumes block sizes to its content; the power panel takes the rest.
    let total_vols = state.volumes.len() + state.unmounted.len();
    let shown_vols = total_vols.min(8);
    let extra_vols = total_vols - shown_vols;
    let vol_h = (shown_vols + extra_vols.min(1)) as u16 + 3;
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(vol_h.max(4)),
            Constraint::Min(0),
        ])
        .split(area);
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(zones[0]);

    let read_title = disk_chart_title(
        "read",
        state.ema_read_bps,
        &state.history_read,
        state.read_iops,
        state.read_lat_ms,
        READ_RAMP[2],
    );
    draw_chart_block(f, halves[0], read_title, &state.history_read, &READ_RAMP, None);
    let write_title = disk_chart_title(
        "write",
        state.ema_write_bps,
        &state.history_write,
        state.write_iops,
        state.write_lat_ms,
        WRITE_RAMP[2],
    );
    draw_chart_block(f, halves[1], write_title, &state.history_write, &WRITE_RAMP, None);

    draw_volumes(f, state, zones[1]);
    draw_power(f, state, zones[2]);
}

// " read 12.3 MB/s · peak 80 MB/s · 450 iops · 0.6 ms" as a chart title.
fn disk_chart_title(
    label: &str,
    ema_bps: f64,
    history: &std::collections::VecDeque<u32>,
    iops: f64,
    lat_ms: f64,
    accent: Color,
) -> Line<'static> {
    let peak = history.iter().copied().max().unwrap_or(0) as f64;
    Line::from(vec![
        Span::styled(
            format!(" {label} {} ", format_bps(ema_bps)),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("peak {} · {:.0} iops · {:.1} ms ", format_bps(peak), iops, lat_ms),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn draw_volumes(f: &mut Frame, state: &HardwareState, area: Rect) {
    let unmounted_part = if state.unmounted.is_empty() {
        String::new()
    } else {
        format!(" · {} unmounted", state.unmounted.len())
    };
    let title = format!(
        " Volumes — {} mounted{unmounted_part} ",
        state.volumes.len(),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(title);
    let inner_width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    if state.volumes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting for first sample …",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let shown = state.volumes.len().min(6);
    for vol in state.volumes.iter().take(shown) {
        let frac = vol.fill_frac();
        // Fill color grades with fullness: comfortable → tight → critical.
        let bar_color = if frac < 0.70 {
            DOWN_RAMP[1]
        } else if frac < 0.90 {
            CPU_RAMP[1]
        } else {
            Color::Rgb(248, 81, 73)
        };
        let filled = ((frac * VOL_BAR_W as f64).round() as usize).min(VOL_BAR_W);
        let name_display = truncate(&vol.name, PROC_NAME_COL);
        let name_pad = PROC_NAME_COL.saturating_sub(name_display.chars().count());
        let sizes = format!(
            "{:>8} of {:>8}",
            format_mem(vol.used()),
            format_mem(vol.total),
        );
        let tag = if vol.removable { "  ⏏ external" } else { "" };
        // Mount path fills whatever is left, dimmed, so rows stay tidy.
        let used_cols =
            2 + PROC_NAME_COL + 1 + VOL_BAR_W + 5 + 2 + sizes.chars().count() + tag.chars().count() + 2;
        let mount_w = inner_width.saturating_sub(used_cols + 2);
        let mount = truncate(&vol.mount, mount_w);
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(
                name_display,
                Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(name_pad + 1)),
            Span::styled("▰".repeat(filled), Style::default().fg(bar_color)),
            Span::styled(
                "▱".repeat(VOL_BAR_W - filled),
                Style::default().fg(Color::Rgb(60, 66, 74)),
            ),
            Span::styled(
                format!(" {:>3.0}%", frac * 100.0),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::styled(sizes, Style::default().fg(Color::Gray)),
        ];
        if !tag.is_empty() {
            spans.push(Span::styled(
                tag.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !mount.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(mount, Style::default().fg(Color::DarkGray)));
        }
        lines.push(Line::from(spans));
    }
    // Unmounted partitions, dimmed: no usage data exists inside them, so
    // the row shows what they are and how much disk they occupy.
    let unmounted_shown = state.unmounted.len().min(8_usize.saturating_sub(shown));
    for vol in state.unmounted.iter().take(unmounted_shown) {
        let name_display = truncate(&vol.name, PROC_NAME_COL);
        let name_pad = PROC_NAME_COL.saturating_sub(name_display.chars().count());
        // "not mounted" sits where mounted rows put their fill bar; the
        // size right-aligns to the same column as "X of Y".
        let bar_slot = format!("{:<w$}", "not mounted", w = VOL_BAR_W + 5);
        let sizes = format!("{:>20}", format_mem(vol.size));
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(name_display, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(name_pad + 1)),
            Span::styled(bar_slot, Style::default().fg(Color::Rgb(60, 66, 74))),
            Span::raw("  "),
            Span::styled(sizes, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(
                format!("{} · {}", vol.device, vol.kind),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        let width_used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if width_used > inner_width {
            spans.truncate(6); // drop the device/kind tag on narrow terminals
        }
        lines.push(Line::from(spans));
    }

    let extra = (state.volumes.len() + state.unmounted.len())
        .saturating_sub(shown + unmounted_shown);
    if extra > 0 {
        lines.push(Line::from(Span::styled(
            format!("  … {extra} more volume{}", if extra == 1 { "" } else { "s" }),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let para = Paragraph::new(lines).style(Style::reset()).block(block);
    f.render_widget(para, area);
}

fn draw_power(f: &mut Frame, state: &HardwareState, area: Rect) {
    let Some(bat) = &state.battery else {
        // Desktop Mac (or first sample pending): no battery telemetry.
        let msg = if state.last_sample_at.is_none() {
            "waiting for first sample …"
        } else {
            "no battery detected — power telemetry needs a portable Mac."
        };
        let para = Paragraph::new(Line::from(Span::styled(
            format!("  {msg}"),
            Style::default().fg(Color::DarkGray),
        )))
        .style(Style::reset())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .style(Style::reset())
                .title(" Power "),
        );
        f.render_widget(para, area);
        return;
    };

    // Battery-flow chart on the left, detail panel on the right. The
    // panel is fixed-width prose; the chart soaks up the rest.
    let panel_w: u16 = 52;
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(panel_w)])
        .split(area);

    draw_battery_flow(f, state, bat, halves[0]);
    draw_battery_panel(f, bat, halves[1]);
}

// Signed battery-flow chart, drawn as a braille line: amber while
// draining, green while charging, hugging the bottom when idle. Sign is
// carried by color; the line's height is the magnitude in watts against
// a round-number scale.
fn draw_battery_flow(f: &mut Frame, state: &HardwareState, bat: &BatterySnapshot, area: Rect) {
    let drain_peak = state.history_watts.iter().copied().min().unwrap_or(0).min(0).unsigned_abs();
    let charge_peak = state.history_watts.iter().copied().max().unwrap_or(0).max(0) as u32;
    // Round the chart ceiling up to a clean number so a steady draw sits
    // at a stable, readable height instead of pinning to the top edge
    // (peak-normalized, a constant 8 W would render as a line glued to
    // the ceiling that rescales on every blip).
    let scale = nice_ceil(drain_peak.max(charge_peak).max(10));

    let (flow_desc, flow_color) = if state.ema_watts < -0.05 {
        (format!("{:.1} W out", -state.ema_watts), CPU_RAMP[2])
    } else if state.ema_watts > 0.05 {
        (format!("{:.1} W in", state.ema_watts), DOWN_RAMP[2])
    } else if bat.external_connected {
        ("idle · on AC".to_string(), Color::DarkGray)
    } else {
        ("idle".to_string(), Color::DarkGray)
    };
    let mut title_spans = vec![Span::styled(
        format!(" battery flow · {flow_desc} "),
        Style::default().fg(flow_color).add_modifier(Modifier::BOLD),
    )];
    if state.history_watts.iter().any(|w| *w != 0) {
        title_spans.push(Span::styled(
            format!(
                "drain peak {:.1} W · charge peak {:.1} W ",
                drain_peak as f64 / 10.0,
                charge_peak as f64 / 10.0,
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Left gutter: watt labels + axis, chart fills the rest. Skip the
    // axis entirely on absurdly narrow panels.
    const GUTTER_W: u16 = 7;
    let rows = inner.height as usize;
    let chart_w = if inner.width > GUTTER_W + 8 {
        (inner.width - GUTTER_W) as usize
    } else {
        inner.width as usize
    };
    let mut lines = render_flow_line(&state.history_watts, chart_w, rows, scale);
    if chart_w < inner.width as usize {
        let ticks = axis_ticks(rows, scale);
        for (r, line) in lines.iter_mut().enumerate() {
            let gutter = match ticks.iter().find(|(row, _)| *row == r) {
                Some((_, watts)) => format!("{:>5} ┤", format_watts_label(*watts)),
                None => "      │".to_string(),
            };
            line.spans
                .insert(0, Span::styled(gutter, Style::default().fg(Color::DarkGray)));
        }
    }
    f.render_widget(Paragraph::new(lines).style(Style::reset()), inner);
}

// Y-axis ticks for the flow chart: nice round watt values (the scale's
// halves, plus quarters once the chart is tall enough) snapped to their
// nearest row. Snapping keeps the labels round — 20/15/10/5/0 — at the
// cost of at most half a row of placement error.
fn axis_ticks(rows: usize, scale_deciwatts: u32) -> Vec<(usize, f64)> {
    let denom = rows.saturating_sub(1).max(1);
    let fracs: &[f64] = if rows >= 14 {
        &[1.0, 0.75, 0.5, 0.25, 0.0]
    } else if rows >= 6 {
        &[1.0, 0.5, 0.0]
    } else {
        &[1.0, 0.0]
    };
    let mut ticks: Vec<(usize, f64)> = Vec::with_capacity(fracs.len());
    for &frac in fracs {
        let row = ((1.0 - frac) * denom as f64).round() as usize;
        if ticks.iter().all(|(r, _)| *r != row) {
            ticks.push((row, scale_deciwatts as f64 / 10.0 * frac));
        }
    }
    ticks
}

// "12" for whole watts, "2.5" when the tick lands on a fraction.
fn format_watts_label(watts: f64) -> String {
    if (watts - watts.round()).abs() < 0.05 {
        format!("{watts:.0}")
    } else {
        format!("{watts:.1}")
    }
}

// Smallest 1/2/5 × 10^k that covers `x` — a chart ceiling that only
// moves when the data genuinely outgrows it.
fn nice_ceil(x: u32) -> u32 {
    let mut m: u32 = 1;
    loop {
        for step in [1, 2, 5] {
            let candidate = m.saturating_mul(step);
            if candidate >= x {
                return candidate;
            }
        }
        m = m.saturating_mul(10);
    }
}

// Plot a signed history as a connected line on a braille canvas (2×4
// dots per cell). Magnitude sets the height; sign sets the color (green
// charge / amber drain, dim gray at zero). Vertical jumps between
// neighbouring samples are filled in so the line reads as continuous.
fn render_flow_line(
    history: &std::collections::VecDeque<i32>,
    width: usize,
    rows: usize,
    scale: u32,
) -> Vec<Line<'static>> {
    let w_dots = width * 2;
    let h_dots = rows * 4;
    let skip = history.len().saturating_sub(w_dots);
    let vals: Vec<i32> = history.iter().skip(skip).copied().collect();
    // Newest sample at the right edge; short histories grow in from it.
    let pad_dots = w_dots - vals.len();
    let scale = scale.max(1);

    // Braille dot bit for (dx, dy) within a cell, per the Unicode layout.
    const DOT_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let mut bits = vec![vec![0u8; width]; rows];
    let mut cell_colors = vec![vec![None::<Color>; width]; rows];
    let mut set_dot = |x: usize, y: usize, color: Color| {
        let (cx, cy) = (x / 2, y / 4);
        if cx < width && cy < rows {
            bits[cy][cx] |= DOT_BITS[x % 2][y % 4];
            cell_colors[cy][cx] = Some(color);
        }
    };

    let mut prev_y: Option<usize> = None;
    for (i, &v) in vals.iter().enumerate() {
        let x = pad_dots + i;
        let ratio = (v.unsigned_abs() as f64 / scale as f64).min(1.0);
        let y = (h_dots - 1) - ((ratio * (h_dots - 1) as f64).round() as usize).min(h_dots - 1);
        let color = if v > 0 {
            DOWN_RAMP[2]
        } else if v < 0 {
            CPU_RAMP[2]
        } else {
            Color::DarkGray
        };
        // Connect to the previous sample's height so steps read as a line.
        let (lo, hi) = match prev_y {
            Some(p) => (y.min(p), y.max(p)),
            None => (y, y),
        };
        for yy in lo..=hi {
            set_dot(x, yy, color);
        }
        prev_y = Some(y);
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(width);
        for cx in 0..width {
            let b = bits[cy][cx];
            if b == 0 {
                spans.push(Span::raw(" "));
            } else {
                let ch = char::from_u32(0x2800 + b as u32).unwrap_or(' ');
                let color = cell_colors[cy][cx].unwrap_or(Color::DarkGray);
                spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

// The gold-plated battery panel: everything interesting the SMC will
// tell us without root, as label/value rows.
fn draw_battery_panel(f: &mut Frame, bat: &BatterySnapshot, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::reset())
        .title(" Battery ");

    let label = |s: &str| Span::styled(format!("  {s:<9}"), Style::default().fg(Color::DarkGray));
    let value = |s: String| Span::styled(s, Style::default().fg(Color::Gray));
    let gauge = |frac: f64, color: Color| -> Vec<Span<'static>> {
        let filled = ((frac * PWR_GAUGE_W as f64).round() as usize).min(PWR_GAUGE_W);
        vec![
            Span::styled("▰".repeat(filled), Style::default().fg(color)),
            Span::styled(
                "▱".repeat(PWR_GAUGE_W - filled),
                Style::default().fg(Color::Rgb(60, 66, 74)),
            ),
        ]
    };

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];

    // State: what the battery is doing right now, plus the time estimate
    // that goes with it.
    let state_desc = if bat.is_charging {
        let eta = bat
            .time_to_full_min
            .map(|m| format!(" · {} to full", format_minutes(m)))
            .unwrap_or_default();
        format!("charging{eta}")
    } else if !bat.external_connected {
        let eta = bat
            .time_to_empty_min
            .map(|m| format!(" · {} left", format_minutes(m)))
            .unwrap_or_default();
        format!("discharging{eta}")
    } else if bat.fully_charged {
        "charged · on AC".to_string()
    } else {
        "on AC · not charging".to_string()
    };
    lines.push(Line::from(vec![label("state"), value(state_desc)]));

    // Adapter: negotiated USB-PD contract + every profile it offered.
    match &bat.adapter {
        Some(a) => {
            let kind = if a.is_wireless { "wireless" } else { "USB-C" };
            lines.push(Line::from(vec![
                label("adapter"),
                value(format!(
                    "{} W {kind} · {:.0} V × {:.1} A",
                    a.watts,
                    a.voltage_mv as f64 / 1000.0,
                    a.current_ma as f64 / 1000.0,
                )),
            ]));
            if !a.profile_volts.is_empty() {
                let profiles = a
                    .profile_volts
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                lines.push(Line::from(vec![
                    label(""),
                    Span::styled(
                        format!("PD profiles {profiles} V"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        None => lines.push(Line::from(vec![
            label("adapter"),
            Span::styled("none · on battery", Style::default().fg(Color::DarkGray)),
        ])),
    }

    // Charge now, in both percent and real mAh.
    let mut charge_spans = vec![label("charge")];
    charge_spans.extend(gauge(bat.percent as f64 / 100.0, DOWN_RAMP[1]));
    charge_spans.push(value(format!(
        " {}% · {} mAh",
        bat.percent, bat.current_capacity_mah,
    )));
    lines.push(Line::from(charge_spans));

    // Health: what a full charge holds today vs the design spec.
    let health = bat.health_percent();
    let health_color = if health >= 80.0 {
        DOWN_RAMP[1]
    } else if health >= 60.0 {
        CPU_RAMP[1]
    } else {
        Color::Rgb(248, 81, 73)
    };
    let mut health_spans = vec![label("health")];
    health_spans.extend(gauge(health / 100.0, health_color));
    health_spans.push(value(format!(
        " {health:.0}% · {} of {} mAh",
        bat.max_capacity_mah, bat.design_capacity_mah,
    )));
    lines.push(Line::from(health_spans));

    lines.push(Line::from(vec![
        label("cycles"),
        value(format!("{}", bat.cycle_count)),
    ]));

    // Pack electricals: live voltage/current and per-cell voltages.
    lines.push(Line::from(vec![
        label("pack"),
        value(format!(
            "{:.3} V · {:+.3} A",
            bat.voltage_mv as f64 / 1000.0,
            bat.amperage_ma as f64 / 1000.0,
        )),
    ]));
    if !bat.cell_voltages_mv.is_empty() {
        let cells = bat
            .cell_voltages_mv
            .iter()
            .map(|mv| format!("{:.3}", *mv as f64 / 1000.0))
            .collect::<Vec<_>>()
            .join(" · ");
        let spread = bat.cell_voltages_mv.iter().max().unwrap_or(&0)
            - bat.cell_voltages_mv.iter().min().unwrap_or(&0);
        lines.push(Line::from(vec![
            label("cells"),
            value(format!("{cells} V")),
            Span::styled(
                format!("  Δ {spread} mV"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Temperature now + the extremes the pack has ever logged.
    let mut temp_line = vec![label("temp"), value(format!("{:.1}°C", bat.temperature_c))];
    if let (Some(lo), Some(hi)) = (bat.lifetime_temp_min_c, bat.lifetime_temp_max_c) {
        temp_line.push(Span::styled(
            format!("  · lifetime {lo}–{hi}°C"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(temp_line));

    // Today's state-of-charge window, from the gauge's daily log.
    if let (Some(lo), Some(hi)) = (bat.daily_min_soc, bat.daily_max_soc) {
        lines.push(Line::from(vec![
            label("today"),
            value(format!("cycled between {lo}% and {hi}%")),
        ]));
    }

    let para = Paragraph::new(lines).style(Style::reset()).block(block);
    f.render_widget(para, area);
}

// Minutes → "2h 14m" / "45m".
fn format_minutes(min: i64) -> String {
    if min >= 60 {
        format!("{}h {:02}m", min / 60, min % 60)
    } else {
        format!("{min}m")
    }
}

