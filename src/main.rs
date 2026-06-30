mod git;

use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use git::{GitTree, spawn_scanner};

const EVENT_POLL: Duration = Duration::from_millis(250);
const HEATMAP_WEEKS_MAX: usize = 27;

struct App {
    git_rx: Receiver<GitTree>,
    git_tree: Option<GitTree>,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            git_rx: spawn_scanner(),
            git_tree: None,
            should_quit: false,
        }
    }

    fn drain_git_updates(&mut self) {
        while let Ok(tree) = self.git_rx.try_recv() {
            self.git_tree = Some(tree);
        }
    }

    fn handle_events(&mut self) -> std::io::Result<()> {
        if event::poll(EVENT_POLL)? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => self.should_quit = true,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
                    _ => {}
                }
            }
        }
        Ok(())
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
        terminal.draw(|f| draw(f, app))?;
        app.handle_events()?;
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(f.area());

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(0)])
        .split(columns[0]);

    draw_heatmap(f, app, left[0]);
    draw_commits(f, app, left[1]);
    draw_right_pane(f, app, columns[1]);
}

fn draw_heatmap(f: &mut Frame, app: &App, area: Rect) {
    let base_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());

    let (title, lines) = match &app.git_tree {
        Some(tree) => render_heatmap(tree, area.width),
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

// Build the 7×N heatmap. Today is in the rightmost (possibly partial) column;
// rows are Sun..Sat top-to-bottom.
fn render_heatmap(tree: &GitTree, area_width: u16) -> (String, Vec<Line<'static>>) {
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
            let count = tree.commit_days.get(&day).copied().unwrap_or(0);
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

    let title = format!(" Commits — {total} in the past year ");
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

fn draw_commits(f: &mut Frame, app: &App, area: Rect) {
    let base_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());

    let (title, lines): (String, Vec<Line<'static>>) = match &app.git_tree {
        Some(tree) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let lines = tree
                .commits
                .iter()
                .map(|c| {
                    let age = format_age_short(now.saturating_sub(c.timestamp));
                    Line::from(vec![
                        Span::styled(
                            format!("{:>5}  ", age),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{:<14}", truncate(&c.repo, 14)),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw("  "),
                        Span::raw(c.subject.clone()),
                    ])
                })
                .collect();
            let title = format!(
                " My commits — {} across {} repos  (q to quit) ",
                tree.commits.len(),
                tree.total_repos,
            );
            (title, lines)
        }
        None => (
            " My commits  (q to quit) ".to_string(),
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

fn draw_right_pane(f: &mut Frame, app: &App, area: Rect) {
    let base_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());

    match &app.git_tree {
        Some(tree) => {
            // Round to 5-second increments so the title doesn't flicker every tick.
            let rounded = (tree.scanned_at.elapsed().as_secs() / 5) * 5;
            let title = format!(
                " Git activity — {} repos in {}  (scanned {}s ago) ",
                tree.total_repos, tree.root_display, rounded,
            );
            let lines = render_git_tree(tree);
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
fn pick_trunk_idx(branches: &[git::BranchInfo]) -> Option<usize> {
    for name in ["main", "master", "develop", "trunk"] {
        if let Some(i) = branches.iter().position(|b| b.name == name) {
            return Some(i);
        }
    }
    None
}

fn render_git_tree(tree: &GitTree) -> Vec<Line<'static>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();

    for repo in &tree.repos {
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
        // all on one line.
        let mut header_spans = vec![
            Span::styled(bullet.to_string(), Style::default().fg(bullet_color)),
            Span::raw(" "),
            Span::styled(
                repo.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
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

        // Top-level indent of 2 spaces.
        let last_idx = rows.len().saturating_sub(1);
        for (i, row) in rows.into_iter().enumerate() {
            render_tree_row(row, "  ", i == last_idx, &mut lines);
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

