mod git;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table},
};
use sysinfo::{CpuRefreshKind, MINIMUM_CPU_UPDATE_INTERVAL, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

use git::{GitTree, spawn_scanner};

const HISTORY_WINDOW_SECS: u64 = 300;
const CHART_INTERVAL_MS: u64 = 500;
const CALLOUT_INTERVAL_MS: u64 = 4_000;
const CPU_CALLOUT_WINDOW_MS: u64 = 4_000;
const NET_CALLOUT_WINDOW_MS: u64 = 10_000;
const PROC_INTERVAL_MS: u64 = 3_000;
const EVENT_POLL_MS: u64 = 250;

const CHART_INTERVAL: Duration = Duration::from_millis(CHART_INTERVAL_MS);
const CALLOUT_INTERVAL: Duration = Duration::from_millis(CALLOUT_INTERVAL_MS);
const PROC_INTERVAL: Duration = Duration::from_millis(PROC_INTERVAL_MS);
const EVENT_POLL: Duration = Duration::from_millis(EVENT_POLL_MS);

const HISTORY_SAMPLES: usize = (HISTORY_WINDOW_SECS * 1000 / CHART_INTERVAL_MS) as usize;
const CPU_CALLOUT_SAMPLES: usize = (CPU_CALLOUT_WINDOW_MS / CHART_INTERVAL_MS) as usize;
const NET_CALLOUT_SAMPLES: usize = (NET_CALLOUT_WINDOW_MS / CHART_INTERVAL_MS) as usize;

const NET_YMAX_FLOOR_MBPS: f64 = 10.0;

struct App {
    system: System,
    networks: Networks,
    cpu_history: VecDeque<f64>,
    net_rx_history: VecDeque<f64>,
    net_tx_history: VecDeque<f64>,
    last_chart_sample: Instant,
    last_net_sample: Instant,
    cpu_callout: f64,
    net_rx_callout: f64,
    net_tx_callout: f64,
    last_callout_update: Instant,
    processes: Vec<ProcessInfo>,
    last_proc_sample: Instant,
    git_rx: Receiver<GitTree>,
    git_tree: Option<GitTree>,
    should_quit: bool,
}

struct ProcessInfo {
    name: String,
    cpu: f32,
    memory_bytes: u64,
}

impl App {
    fn new() -> Self {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_processes(ProcessRefreshKind::everything()),
        );

        // CPU usage is computed as a delta between two samples — the first
        // reading is always zero, so prime it before the UI starts drawing.
        system.refresh_cpu_all();
        std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);

        let networks = Networks::new_with_refreshed_list();
        let git_rx = spawn_scanner();

        let now = Instant::now();
        let mut app = Self {
            system,
            networks,
            cpu_history: VecDeque::with_capacity(HISTORY_SAMPLES),
            net_rx_history: VecDeque::with_capacity(HISTORY_SAMPLES),
            net_tx_history: VecDeque::with_capacity(HISTORY_SAMPLES),
            last_chart_sample: now,
            last_net_sample: now,
            cpu_callout: 0.0,
            net_rx_callout: 0.0,
            net_tx_callout: 0.0,
            last_callout_update: now,
            processes: Vec::new(),
            last_proc_sample: now,
            git_rx,
            git_tree: None,
            should_quit: false,
        };
        app.sample_cpu();
        app.sample_network();
        app.sample_processes();
        app.update_callouts();
        app
    }

    fn sample_cpu(&mut self) {
        self.system.refresh_cpu_all();
        let cpu = self.system.global_cpu_usage() as f64;
        push_bounded(&mut self.cpu_history, cpu);
        self.last_chart_sample = Instant::now();
    }

    fn sample_network(&mut self) {
        let now = Instant::now();
        // Guard against a zero / near-zero divisor on the first sample.
        let elapsed = (now - self.last_net_sample).as_secs_f64().max(0.001);
        self.networks.refresh(true);

        let (mut rx_bytes, mut tx_bytes) = (0u64, 0u64);
        for (_, data) in &self.networks {
            rx_bytes += data.received();
            tx_bytes += data.transmitted();
        }

        let rx_mbps = bytes_to_mbps(rx_bytes, elapsed);
        let tx_mbps = bytes_to_mbps(tx_bytes, elapsed);
        push_bounded(&mut self.net_rx_history, rx_mbps);
        push_bounded(&mut self.net_tx_history, tx_mbps);
        self.last_net_sample = now;
    }

    fn sample_processes(&mut self) {
        self.system
            .refresh_processes(ProcessesToUpdate::All, true);

        // Collapse processes sharing a name (e.g. all "Google Chrome Helper"
        // workers) into one row, summing CPU% and memory.
        let mut agg: HashMap<String, (f32, u64)> = HashMap::new();
        for p in self.system.processes().values() {
            let name = p.name().to_string_lossy().into_owned();
            let entry = agg.entry(name).or_insert((0.0, 0));
            entry.0 += p.cpu_usage();
            entry.1 += p.memory();
        }

        let mut procs: Vec<ProcessInfo> = agg
            .into_iter()
            .map(|(name, (cpu, memory_bytes))| ProcessInfo {
                name,
                cpu,
                memory_bytes,
            })
            .collect();
        procs.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        procs.truncate(10);
        self.processes = procs;
        self.last_proc_sample = Instant::now();
    }

    fn update_callouts(&mut self) {
        self.cpu_callout = average_tail(&self.cpu_history, CPU_CALLOUT_SAMPLES);
        self.net_rx_callout = average_tail(&self.net_rx_history, NET_CALLOUT_SAMPLES);
        self.net_tx_callout = average_tail(&self.net_tx_history, NET_CALLOUT_SAMPLES);
        self.last_callout_update = Instant::now();
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

fn push_bounded(buf: &mut VecDeque<f64>, value: f64) {
    if buf.len() == HISTORY_SAMPLES {
        buf.pop_front();
    }
    buf.push_back(value);
}

fn average_tail(buf: &VecDeque<f64>, n: usize) -> f64 {
    let take = n.min(buf.len());
    if take == 0 {
        return 0.0;
    }
    let sum: f64 = buf.iter().rev().take(take).sum();
    sum / take as f64
}

fn bytes_to_mbps(bytes: u64, seconds: f64) -> f64 {
    (bytes as f64 * 8.0) / (seconds * 1_000_000.0)
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
        if app.last_chart_sample.elapsed() >= CHART_INTERVAL {
            app.sample_cpu();
            app.sample_network();
        }
        if app.last_callout_update.elapsed() >= CALLOUT_INTERVAL {
            app.update_callouts();
        }
        if app.last_proc_sample.elapsed() >= PROC_INTERVAL {
            app.sample_processes();
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(f.area());

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Max(13)])
        .split(columns[0]);

    let charts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(left[0]);

    draw_cpu_chart(f, app, charts[0]);
    draw_network_chart(f, app, charts[1]);
    draw_process_table(f, app, left[1]);
    draw_right_pane(f, app, columns[1]);
}

fn history_to_points(history: &VecDeque<f64>) -> Vec<(f64, f64)> {
    let len = history.len();
    let interval = CHART_INTERVAL.as_secs_f64();
    history
        .iter()
        .enumerate()
        .map(|(i, &v)| (-((len - 1 - i) as f64) * interval, v))
        .collect()
}

fn draw_cpu_chart(f: &mut Frame, app: &App, area: Rect) {
    let data = history_to_points(&app.cpu_history);

    let datasets = vec![
        Dataset::default()
            .name("CPU %")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&data),
    ];

    let title = format!(
        " CPU usage — {:.1}% (4s avg, last 5 min) ",
        app.cpu_callout
    );
    let chart = Chart::new(datasets)
        .style(Style::reset())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::reset())
                .title(title),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-(HISTORY_WINDOW_SECS as f64), 0.0])
                .labels(vec!["-5m", "-2.5m", "now"]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, 100.0])
                .labels(vec!["0%", "50%", "100%"]),
        );

    f.render_widget(chart, area);
}

fn draw_network_chart(f: &mut Frame, app: &App, area: Rect) {
    let rx_data = history_to_points(&app.net_rx_history);
    let tx_data = history_to_points(&app.net_tx_history);

    // Auto-scale y-axis to the largest value in the window, with a floor so
    // the chart doesn't look jumpy when traffic is near zero.
    let peak = app
        .net_rx_history
        .iter()
        .chain(app.net_tx_history.iter())
        .copied()
        .fold(0.0_f64, f64::max);
    let ymax = (peak * 1.2).max(NET_YMAX_FLOOR_MBPS);

    let datasets = vec![
        Dataset::default()
            .name("↓ rx")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&rx_data),
        Dataset::default()
            .name("↑ tx")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Magenta))
            .data(&tx_data),
    ];

    let title = format!(
        " Network — ↓ {:.1} Mbps  ↑ {:.1} Mbps  (10s avg) ",
        app.net_rx_callout, app.net_tx_callout
    );
    let y_labels: Vec<String> = vec![
        "0".to_string(),
        format!("{:.0}", ymax / 2.0),
        format!("{:.0}", ymax),
    ];

    let chart = Chart::new(datasets)
        .style(Style::reset())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::reset())
                .title(title),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([-(HISTORY_WINDOW_SECS as f64), 0.0])
                .labels(vec!["-5m", "-2.5m", "now"]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, ymax])
                .labels(y_labels),
        );

    f.render_widget(chart, area);
}

fn draw_process_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec!["NAME", "CPU %", "MEMORY"]).style(Style::reset());

    let rows: Vec<Row> = app
        .processes
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(format!("{:.1}%", p.cpu)),
                Cell::from(format_bytes(p.memory_bytes)),
            ])
            .style(Style::reset())
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .style(Style::reset())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::reset())
                .title(" Top 10 by CPU  (q to quit) "),
        );

    f.render_widget(table, area);
}

fn draw_right_pane(f: &mut Frame, app: &App, area: Rect) {
    let base_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::reset());

    match &app.git_tree {
        Some(tree) => {
            let title = format!(
                " Git activity — {} repos in {}  (scanned {}s ago) ",
                tree.total_repos,
                tree.root_display,
                tree.scanned_at.elapsed().as_secs(),
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
        let bullet = match age_secs {
            Some(s) if s < 86_400 => "●",
            Some(_) => "○",
            None => "·",
        };
        let bullet_color = match age_secs {
            Some(s) if s < 86_400 => Color::Green,
            Some(_) => Color::DarkGray,
            None => Color::DarkGray,
        };
        let dirty_marker = if repo.is_dirty { "  ◇ dirty" } else { "" };

        lines.push(Line::from(vec![
            Span::styled(bullet.to_string(), Style::default().fg(bullet_color)),
            Span::raw(" "),
            Span::styled(
                repo.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {age_str}"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(dirty_marker.to_string(), Style::default().fg(Color::Yellow)),
        ]));

        if repo.branches.is_empty() {
            lines.push(Line::from(Span::styled(
                "      (no commits)".to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        }

        for branch in &repo.branches {
            let branch_age = branch
                .last_commit
                .map(|t| format_age(now.saturating_sub(t)))
                .unwrap_or_default();
            let marker = if branch.is_current { "*" } else { " " };
            let track = match (branch.ahead, branch.behind) {
                (0, 0) => String::new(),
                (a, 0) => format!("  ↑{a}"),
                (0, b) => format!("  ↓{b}"),
                (a, b) => format!("  ↑{a} ↓{b}"),
            };
            let name_style = if branch.is_current {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {marker} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(branch.name.clone(), name_style),
                Span::styled(track, Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("  {branch_age}  "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    truncate(&branch.last_message, 40),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
