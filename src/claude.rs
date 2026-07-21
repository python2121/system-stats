//! Claude Code session browser for the "Claude" tab.
//!
//! Claude Code stores one transcript per session as JSONL under
//! `~/.claude/projects/<munged-project-path>/<session-id>.jsonl`, where the
//! munged name is the absolute project path with every non-alphanumeric
//! character replaced by `-`. Live (currently running) sessions additionally
//! publish a status file at `~/.claude/sessions/<pid>.json`.
//!
//! The scanner maps each directory under the watch dir to its transcript
//! store, extracts a per-session summary (first typed prompt as the title,
//! prompt count, git branch, last-activity mtime), and joins in the live
//! registry by session id. Transcript parsing is cached by (mtime, size) so
//! steady-state scans are just directory stats.
//!
//! The JSONL shape is Claude Code's internal format, not a documented API —
//! every extraction here is defensive: unparseable lines are skipped and the
//! file mtime still gives a usable row.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;

// Steady-state cadence; halved while the Claude tab is focused (steady-
// state scans are just directory stats thanks to the mtime cache, so the
// faster cadence is cheap).
const SCAN_INTERVAL: Duration = Duration::from_secs(10);
// Titles are truncated again by the UI to fit the pane; this just bounds
// what we keep in memory per session.
const TITLE_MAX_CHARS: usize = 120;

pub struct ClaudeTree {
    // The watch dir this tree was scanned from — same stale-root guard as
    // GitTree, so a scan in flight across a settings change gets dropped.
    pub root: PathBuf,
    pub root_display: String,
    pub projects: Vec<ProjectSessions>,
    pub total_sessions: usize,
    pub live_count: usize,
    pub total_cost_usd: f64,
    pub scanned_at: Instant,
}

pub struct ProjectSessions {
    pub name: String,
    pub path: PathBuf,
    // Newest first.
    pub sessions: Vec<SessionInfo>,
    // Epoch seconds of the most recent session activity.
    pub last_activity: Option<u64>,
    pub total_cost_usd: f64,
}

#[derive(Clone)]
pub struct SessionInfo {
    pub id: String,
    // Claude Code's generated title when the transcript has one, else the
    // first typed user prompt, whitespace-collapsed. Empty when neither
    // was found — the UI falls back to the session id.
    pub title: String,
    // The most recent typed prompt — "what was this session last doing".
    pub last_prompt: String,
    // Transcript file mtime, epoch seconds.
    pub last_activity: u64,
    // Number of human prompts (tool results and sidechains excluded).
    pub prompt_count: usize,
    pub git_branch: Option<String>,
    // Wall-clock span between the first and last record. 0 when unknown.
    pub duration_secs: u64,
    // Total output tokens across the session's assistant messages —
    // a "how much work happened here" number.
    pub output_tokens: u64,
    // API-equivalent cost in USD, computed from each message's usage block
    // and the MODEL_RATES table. Notional for subscription users — what the
    // same usage would have billed at API rates.
    pub cost_usd: f64,
    // Tool invocations by name, most-used first — the session's activity
    // profile (lots of Edit = building, lots of WebSearch = research).
    pub tool_counts: Vec<(String, u32)>,
    // Models that produced the session's assistant messages, by message
    // count, most-used first. Usually one entry; more when the session
    // mixed models (subagents, /model switches, fallbacks).
    pub models: Vec<(String, u32)>,
    // Present when a running Claude Code process owns this session.
    pub live: Option<LiveSession>,
}

#[derive(Clone)]
pub struct LiveSession {
    pub pid: u32,
    // Claude Code's derived session name, e.g. "system-stats-0c".
    pub name: String,
    // "busy", "idle", …  — whatever the status file reports.
    pub status: String,
}

// Owns the scanner thread — the same two-channel shape as git::Scanner:
// new trees arrive on tree_rx; set_root swaps the watched path and pokes
// the wake channel so the rescan happens now instead of next tick.
pub struct Scanner {
    tree_rx: Receiver<ClaudeTree>,
    wake_tx: Sender<()>,
    root: Arc<Mutex<PathBuf>>,
    focused: Arc<AtomicBool>,
}

impl Scanner {
    pub fn try_recv(&self) -> Result<ClaudeTree, mpsc::TryRecvError> {
        self.tree_rx.try_recv()
    }

    pub fn set_root(&self, new_root: PathBuf) {
        if let Ok(mut guard) = self.root.lock() {
            *guard = new_root;
        }
        let _ = self.wake_tx.send(());
    }

    // Claude tab focus: scan twice as often while the user is looking at
    // it, and kick off a scan right away on gaining focus so the tab
    // opens on fresh data instead of up-to-10s-old data.
    pub fn set_focused(&self, focused: bool) {
        let was = self.focused.swap(focused, Ordering::Relaxed);
        if focused && !was {
            let _ = self.wake_tx.send(());
        }
    }
}

pub fn spawn_scanner(initial_root: PathBuf) -> Scanner {
    let (tree_tx, tree_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let root = Arc::new(Mutex::new(initial_root));
    let thread_root = Arc::clone(&root);
    let focused = Arc::new(AtomicBool::new(false));
    let thread_focused = Arc::clone(&focused);
    thread::spawn(move || {
        // Parsed-transcript cache keyed by file path; entries are reused
        // while (mtime, size) match so only actively-written transcripts
        // get re-read.
        let mut cache: HashMap<PathBuf, CacheEntry> = HashMap::new();
        loop {
            let current = thread_root.lock().ok().map(|g| g.clone());
            if let Some(root) = current {
                let tree = scan(&root, &mut cache);
                if tree_tx.send(tree).is_err() {
                    // UI dropped the receiver; we're done.
                    return;
                }
            }
            let interval = if thread_focused.load(Ordering::Relaxed) {
                SCAN_INTERVAL / 2
            } else {
                SCAN_INTERVAL
            };
            match wake_rx.recv_timeout(interval) {
                Ok(()) => {
                    // Collapse a burst of set_root calls into one rescan.
                    while wake_rx.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    Scanner { tree_rx, wake_tx, root, focused }
}

struct CacheEntry {
    mtime: SystemTime,
    len: u64,
    parsed: ParsedTranscript,
}

// One assistant message's token usage, from its `usage` block.
#[derive(Clone, Copy, Default)]
struct MsgUsage {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

// API pricing in USD per million tokens (input, output), matched by model-id
// prefix so new versions within a family inherit their tier's rates. Cache
// writes bill at 1.25 × input (5-minute TTL assumed — 1-hour writes are 2×,
// so this can slightly underestimate) and cache reads at 0.1 × input.
// Prices change occasionally; update alongside Anthropic's published
// pricing. Cached 2026-07.
const MODEL_RATES: [(&str, f64, f64); 5] = [
    ("claude-fable", 10.0, 50.0),
    ("claude-mythos", 10.0, 50.0),
    ("claude-opus", 5.0, 25.0),
    ("claude-sonnet", 3.0, 15.0),
    ("claude-haiku", 1.0, 5.0),
];

fn model_rates(model: &str) -> (f64, f64) {
    for (prefix, input, output) in MODEL_RATES {
        if model.starts_with(prefix) {
            return (input, output);
        }
    }
    // Unknown model — assume Opus-tier rather than pricing it at zero.
    (5.0, 25.0)
}

fn usage_cost(model: &str, u: &MsgUsage) -> f64 {
    let (input_rate, output_rate) = model_rates(model);
    (u.input as f64 * input_rate
        + u.output as f64 * output_rate
        + u.cache_write as f64 * input_rate * 1.25
        + u.cache_read as f64 * input_rate * 0.10)
        / 1e6
}

#[derive(Clone, Default)]
struct ParsedTranscript {
    // First typed prompt — the fallback title for older transcripts.
    title: String,
    // Claude Code's own generated title; appended repeatedly as the
    // conversation evolves, so the last occurrence wins.
    ai_title: String,
    last_prompt: String,
    prompt_count: usize,
    git_branch: Option<String>,
    duration_secs: u64,
    output_tokens: u64,
    cost_usd: f64,
    tool_counts: Vec<(String, u32)>,
    models: Vec<(String, u32)>,
}

fn scan(root: &Path, cache: &mut HashMap<PathBuf, CacheEntry>) -> ClaudeTree {
    let store_root = claude_dir().map(|d| d.join("projects"));
    let live = read_live_sessions();

    let mut projects: Vec<ProjectSessions> = Vec::new();
    if let (Some(store_root), Ok(rd)) = (&store_root, fs::read_dir(root)) {
        let mut fresh: HashMap<PathBuf, CacheEntry> = HashMap::new();
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !path.is_dir() || name.starts_with('.') {
                continue;
            }
            let store = store_root.join(munge_path(&path));
            let Ok(files) = fs::read_dir(&store) else { continue };

            let mut sessions: Vec<SessionInfo> = Vec::new();
            for f in files.flatten() {
                let fpath = f.path();
                if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(id) = fpath
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                else {
                    continue;
                };
                let Ok(meta) = f.metadata() else { continue };
                let Ok(mtime) = meta.modified() else { continue };
                let len = meta.len();
                let parsed = match cache.remove(&fpath) {
                    Some(e) if e.mtime == mtime && e.len == len => e.parsed,
                    _ => parse_transcript(&fpath),
                };
                fresh.insert(
                    fpath.clone(),
                    CacheEntry { mtime, len, parsed: parsed.clone() },
                );
                let last_activity = mtime
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                sessions.push(SessionInfo {
                    live: live.get(&id).cloned(),
                    id,
                    // Prefer the generated title; older transcripts only
                    // have the first prompt to go on.
                    title: if parsed.ai_title.is_empty() {
                        parsed.title
                    } else {
                        parsed.ai_title
                    },
                    last_prompt: parsed.last_prompt,
                    last_activity,
                    prompt_count: parsed.prompt_count,
                    git_branch: parsed.git_branch,
                    duration_secs: parsed.duration_secs,
                    output_tokens: parsed.output_tokens,
                    cost_usd: parsed.cost_usd,
                    tool_counts: parsed.tool_counts,
                    models: parsed.models,
                });
            }
            if sessions.is_empty() {
                continue;
            }
            sessions.sort_by(|a, b| {
                b.last_activity
                    .cmp(&a.last_activity)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let last_activity = sessions.iter().map(|s| s.last_activity).max();
            let total_cost_usd = sessions.iter().map(|s| s.cost_usd).sum();
            projects.push(ProjectSessions {
                name,
                path,
                sessions,
                last_activity,
                total_cost_usd,
            });
        }
        // Entries for deleted transcripts (or a changed root) fall away here.
        *cache = fresh;
    }

    projects.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.name.cmp(&b.name))
    });
    let total_sessions = projects.iter().map(|p| p.sessions.len()).sum();
    let live_count = projects
        .iter()
        .flat_map(|p| &p.sessions)
        .filter(|s| s.live.is_some())
        .count();
    let total_cost_usd = projects.iter().map(|p| p.total_cost_usd).sum();

    ClaudeTree {
        root: root.to_path_buf(),
        root_display: config::display_path(root),
        projects,
        total_sessions,
        live_count,
        total_cost_usd,
        scanned_at: Instant::now(),
    }
}

fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

// Open a new window in the user's terminal app, cd into `dir`, and
// optionally run `command` there. When `command` is None the window is
// left at a plain interactive shell; when Some it's typed into the shell
// and run after the `cd`, leaving the shell behind when it exits. The
// string must already be shell-ready (quoted as needed) — it's dropped in
// verbatim. Fire-and-forget: the launcher process is spawned detached and
// failures are silent — the new window (or its absence) is its own feedback.
#[cfg(target_os = "macos")]
pub fn open_in_terminal(
    terminal: config::TerminalApp,
    dir: &Path,
    command: Option<&str>,
) {
    let dir = dir.display().to_string();
    // iTerm/Terminal run a single line in a fresh shell: cd first, then the
    // optional command joined with `&&` so it only runs if the cd succeeds.
    let shell_cmd = match command {
        Some(c) => format!("cd {} && {}", shell_quote(&dir), c),
        None => format!("cd {}", shell_quote(&dir)),
    };

    let mut cmd = match terminal {
        // Ghostty ≥ 1.3 ships an AppleScript dictionary. Scripting the
        // running app avoids `open -na`, which spawns a whole second app
        // instance; and typing the command via `initial input` (rather
        // than `command`, which replaces the shell) leaves a normal
        // interactive shell behind when the command ends.
        config::TerminalApp::Ghostty => {
            // Ghostty takes the working dir as config; only the command (if
            // any) is typed into the shell, so no `cd` line is needed here.
            let input_line = match command {
                Some(c) => format!(
                    "set initial input of cfg to \"{}\" & linefeed\n",
                    applescript_escape(c),
                ),
                None => String::new(),
            };
            let script = format!(
                "tell application \"Ghostty\"\n\
                 activate\n\
                 set cfg to new surface configuration\n\
                 set initial working directory of cfg to \"{}\"\n\
                 {}\
                 new window with configuration cfg\n\
                 end tell",
                applescript_escape(&dir),
                input_line,
            );
            let mut c = Command::new("osascript");
            c.args(["-e", &script]);
            c
        }
        // iTerm: create a window, then type the command into its shell —
        // the window survives after the command ends.
        config::TerminalApp::Iterm2 => {
            let script = format!(
                "tell application \"iTerm\"\n\
                 activate\n\
                 set newWindow to (create window with default profile)\n\
                 tell current session of newWindow to write text \"{}\"\n\
                 end tell",
                applescript_escape(&shell_cmd),
            );
            let mut c = Command::new("osascript");
            c.args(["-e", &script]);
            c
        }
        // Terminal.app: `do script` with no target opens a new window.
        config::TerminalApp::TerminalApp => {
            let script = format!(
                "tell application \"Terminal\"\n\
                 activate\n\
                 do script \"{}\"\n\
                 end tell",
                applescript_escape(&shell_cmd),
            );
            let mut c = Command::new("osascript");
            c.args(["-e", &script]);
            c
        }
    };

    if let Ok(mut child) = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        // Reap the launcher on a background thread so it doesn't linger
        // as a zombie for the app's lifetime.
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

// Terminal-window scripting is AppleScript-only for now, so on other
// platforms the "open terminal here" / "new session" actions are silent
// no-ops. A Linux launcher (gnome-terminal/kitty/alacritty spawns) would
// slot in here without touching any caller.
#[cfg(not(target_os = "macos"))]
pub fn open_in_terminal(
    _terminal: config::TerminalApp,
    _dir: &Path,
    _command: Option<&str>,
) {
}

// Open a new terminal window, cd into the project, and re-attach to a
// session with `claude --resume`. Thin wrapper over open_in_terminal.
pub fn resume_in_terminal(
    terminal: config::TerminalApp,
    project_dir: &Path,
    session_id: &str,
) {
    let command = format!("claude --resume {}", shell_quote(session_id));
    open_in_terminal(terminal, project_dir, Some(&command));
}

// Single-quote for POSIX shells: '…' with embedded quotes as '\''.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// Escape for embedding inside an AppleScript double-quoted string.
#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

// "/Users/andrew/Documents/code/my.app" → "-Users-andrew-Documents-code-my-app".
// Claude Code names each project's transcript directory this way: every
// character that isn't ASCII-alphanumeric becomes '-'.
fn munge_path(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// Live-session registry: one JSON status file per running Claude Code
// process. Keyed by session id for the join against transcripts. A file
// whose pid is dead is a leftover from a crash — skip it.
fn read_live_sessions() -> HashMap<String, LiveSession> {
    let mut out = HashMap::new();
    let Some(dir) = claude_dir() else { return out };
    let Ok(rd) = fs::read_dir(dir.join("sessions")) else { return out };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Some(session_id) = json_str(&text, "sessionId") else { continue };
        let Some(pid) = json_u64(&text, "pid") else { continue };
        if !pid_alive(pid as u32) {
            continue;
        }
        out.insert(
            session_id,
            LiveSession {
                pid: pid as u32,
                name: json_str(&text, "name").unwrap_or_default(),
                status: json_str(&text, "status")
                    .unwrap_or_else(|| "unknown".to_string()),
            },
        );
    }
    out
}

// `kill -0` sends no signal, just checks deliverability — the portable
// "is this pid alive" probe. Sessions are our own user's processes, so
// permission failures don't false-negative here.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parse_transcript(path: &Path) -> ParsedTranscript {
    let Ok(bytes) = fs::read(path) else { return ParsedTranscript::default() };
    parse_transcript_text(&String::from_utf8_lossy(&bytes))
}

// One JSON record per line, scanned in a single pass with substring probes
// before any extraction so multi-megabyte transcripts stay cheap. Gathers:
// titles (generated + first prompt), the last prompt, prompt count, git
// branch, wall-clock span, output-token total, and tool-use counts.
fn parse_transcript_text(text: &str) -> ParsedTranscript {
    let mut out = ParsedTranscript::default();
    // Assistant messages are re-written once per content block, each copy
    // carrying the same message id and a usage snapshot — summing lines
    // naively would count one message several times over. Dedupe by
    // message id (last snapshot wins) and by tool-use id.
    let mut usage_by_msg: HashMap<String, (String, MsgUsage)> = HashMap::new();
    let mut stray_tokens: u64 = 0;
    let mut stray_cost: f64 = 0.0;
    let mut seen_tool_ids: HashSet<String> = HashSet::new();
    let mut tool_counts: HashMap<String, u32> = HashMap::new();
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;

    for line in text.lines() {
        if let Some(ts) = extract_timestamp(line) {
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
            last_ts = Some(ts);
        }
        if let Some(b) = json_str(line, "gitBranch") {
            if !b.is_empty() {
                out.git_branch = Some(b);
            }
        }
        if line.contains("\"type\":\"ai-title\"") {
            if let Some(t) = json_str(line, "aiTitle") {
                let collapsed = collapse_ws(&t);
                if !collapsed.is_empty() {
                    out.ai_title = collapsed;
                }
            }
            continue;
        }
        if line.contains("\"type\":\"last-prompt\"") {
            if let Some(t) = json_str(line, "lastPrompt") {
                let collapsed = collapse_ws(&t);
                if !collapsed.is_empty() {
                    out.last_prompt = collapsed;
                }
            }
            continue;
        }
        if line.contains("\"type\":\"assistant\"") {
            // Sidechain (subagent) messages are included on purpose:
            // tokens and tool calls measure total work done, unlike the
            // prompt count below which measures the human conversation.
            if line.contains("\"output_tokens\":") {
                let usage = MsgUsage {
                    input: json_u64(line, "input_tokens").unwrap_or(0),
                    output: json_u64(line, "output_tokens").unwrap_or(0),
                    cache_write: json_u64(line, "cache_creation_input_tokens")
                        .unwrap_or(0),
                    cache_read: json_u64(line, "cache_read_input_tokens")
                        .unwrap_or(0),
                };
                let model = json_str(line, "model").unwrap_or_default();
                match message_id(line) {
                    Some(id) => {
                        usage_by_msg.insert(id, (model, usage));
                    }
                    None => {
                        stray_tokens += usage.output;
                        stray_cost += usage_cost(&model, &usage);
                    }
                }
            }
            let pat = "\"type\":\"tool_use\"";
            let mut from = 0;
            while let Some(pos) = line[from..].find(pat) {
                let slice = &line[from + pos..];
                from += pos + pat.len();
                let (Some(id), Some(name)) =
                    (json_str(slice, "id"), json_str(slice, "name"))
                else {
                    continue;
                };
                if seen_tool_ids.insert(id) {
                    *tool_counts.entry(name).or_insert(0) += 1;
                }
            }
            continue;
        }
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        // Not conversation: subagent traffic, injected context, tool-result
        // carrier records, and post-compaction continuation prompts.
        if line.contains("\"isSidechain\":true")
            || line.contains("\"isMeta\":true")
            || line.contains("\"tool_result\"")
            || line.contains("\"isCompactSummary\":true")
        {
            continue;
        }
        out.prompt_count += 1;
        if out.title.is_empty() {
            if let Some(t) = extract_user_text(line) {
                let cleaned = clean_title(&t);
                if !cleaned.is_empty() {
                    out.title = cleaned;
                }
            }
        }
    }

    out.output_tokens = stray_tokens;
    out.cost_usd = stray_cost;
    let mut model_counts: HashMap<String, u32> = HashMap::new();
    for (model, usage) in usage_by_msg.values() {
        out.output_tokens += usage.output;
        out.cost_usd += usage_cost(model, usage);
        if !model.is_empty() {
            *model_counts.entry(model.clone()).or_insert(0) += 1;
        }
    }
    let mut models: Vec<(String, u32)> = model_counts.into_iter().collect();
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.models = models;
    out.duration_secs = match (first_ts, last_ts) {
        (Some(a), Some(b)) => b.saturating_sub(a),
        _ => 0,
    };
    let mut counts: Vec<(String, u32)> = tool_counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.tool_counts = counts;
    out
}

// The API message id ("id":"msg_…") that a re-written assistant record
// shares across its copies. None for synthetic records without one.
fn message_id(line: &str) -> Option<String> {
    let at = line.find("\"id\":\"msg_")?;
    json_str(&line[at..], "id")
}

// Epoch seconds from a record's `"timestamp":"2026-07-06T05:58:14.438Z"`.
// Timestamps are always UTC with that fixed layout, so slicing the first
// 19 chars avoids allocating per line.
fn extract_timestamp(line: &str) -> Option<u64> {
    let pat = "\"timestamp\":\"";
    let at = line.find(pat)? + pat.len();
    parse_iso_secs(line.get(at..at + 19)?)
}

fn parse_iso_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec).ok()
}

// Howard Hinnant's days_from_civil — the inverse of civil_from_days in
// main.rs. Days are signed from 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// User message content is either a plain string ("content":"…") or an array
// of blocks ([{"type":"text","text":"…"}]). Try the string shape first.
fn extract_user_text(line: &str) -> Option<String> {
    if let Some(at) = line.find("\"content\":\"") {
        return json_str(&line[at..], "content");
    }
    if let Some(at) = line.find("\"text\":\"") {
        return json_str(&line[at..], "text");
    }
    None
}

// Collapse runs of whitespace to single spaces and bound the length.
fn collapse_ws(raw: &str) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > TITLE_MAX_CHARS {
        let cut: String = collapsed.chars().take(TITLE_MAX_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        collapsed
    }
}

// collapse_ws plus non-prompt filtering: slash-command wrappers arrive as
// XML-ish "<command-name>…" content, and interrupted/resumed sessions
// inject bracketed or "Caveat:"-prefixed system text.
fn clean_title(raw: &str) -> String {
    let collapsed = collapse_ws(raw);
    if collapsed.starts_with('<')
        || collapsed.starts_with('[')
        || collapsed.starts_with("Caveat:")
        || collapsed.starts_with("This session is being continued")
    {
        return String::new();
    }
    collapsed
}

// First string value following `"key":"` with JSON escapes decoded.
// Collection stops early past what any caller displays, so a pathological
// multi-kilobyte value can't balloon memory.
fn json_str(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = text.find(&pat)? + pat.len();
    let mut out = String::new();
    let mut chars = text[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' | 't' | 'r' => out.push(' '),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(ch) =
                        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                    {
                        out.push(ch);
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
        if out.len() > 600 {
            return Some(out);
        }
    }
    None
}

// First integer following `"key":` — for the pid field in status files.
fn json_u64(text: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let start = text.find(&pat)? + pat.len();
    let rest = text[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/Users/a/My Code"), "'/Users/a/My Code'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_quoting() {
        assert_eq!(
            applescript_escape(r#"cd '/a/b' && claude --resume 'x"y'"#),
            r#"cd '/a/b' && claude --resume 'x\"y'"#,
        );
        assert_eq!(applescript_escape(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn munges_paths_like_claude_code() {
        assert_eq!(
            munge_path(Path::new("/Users/andrew/Documents/code/system-stats")),
            "-Users-andrew-Documents-code-system-stats",
        );
        // Dots and spaces both collapse to '-'.
        assert_eq!(
            munge_path(Path::new("/Volumes/My Disk/app.v2")),
            "-Volumes-My-Disk-app-v2",
        );
    }

    #[test]
    fn json_str_decodes_escapes() {
        let text = r#"{"content":"line\nbreak \"q\" A \\slash"}"#;
        assert_eq!(
            json_str(text, "content").unwrap(),
            r#"line break "q" A \slash"#,
        );
        assert_eq!(json_str(text, "missing"), None);
    }

    #[test]
    fn json_u64_parses_pid() {
        assert_eq!(json_u64(r#"{"pid":71530,"x":1}"#, "pid"), Some(71530));
        assert_eq!(json_u64(r#"{"pid":"nope"}"#, "pid"), None);
    }

    const TRANSCRIPT: &str = r#"{"type":"mode","mode":"normal","sessionId":"abc"}
{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"make me a TUI \"dashboard\"   please"},"cwd":"/Users/x/proj","sessionId":"abc","gitBranch":"main"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"gitBranch":"main"}
{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"tool output"}]}}
{"type":"user","message":{"role":"user","content":"second real prompt"},"gitBranch":"feature/x"}
"#;

    #[test]
    fn transcript_title_count_and_branch() {
        let p = parse_transcript_text(TRANSCRIPT);
        // Title comes from the first real prompt, whitespace collapsed,
        // escapes decoded.
        assert_eq!(p.title, "make me a TUI \"dashboard\" please");
        // Sidechain and tool-result records don't count as prompts.
        assert_eq!(p.prompt_count, 2);
        // Branch reflects the last record that carried one.
        assert_eq!(p.git_branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn command_and_caveat_records_never_become_titles() {
        let text = concat!(
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"Caveat: injected system text"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"the actual ask"}}"#,
            "\n",
        );
        let p = parse_transcript_text(text);
        assert_eq!(p.title, "the actual ask");
        assert_eq!(p.prompt_count, 3);
    }

    // A session with the modern record types: ai-title / last-prompt
    // markers, and an assistant message re-written per content block
    // (same msg id, same tool id, evolving usage snapshot).
    const RICH_TRANSCRIPT: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"build the thing"},"timestamp":"2026-07-06T05:58:14.438Z","gitBranch":"main"}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_01AAA","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}],"usage":{"input_tokens":10,"output_tokens":100}},"timestamp":"2026-07-06T05:59:00.000Z"}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg_01AAA","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}},{"type":"tool_use","id":"toolu_2","name":"Edit","input":{}}],"usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":10000,"output_tokens":150}},"timestamp":"2026-07-06T05:59:10.000Z"}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-fable-5","id":"msg_01BBB","role":"assistant","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":5,"output_tokens":50}},"timestamp":"2026-07-06T06:58:14.000Z"}"#,
        "\n",
        r#"{"type":"ai-title","aiTitle":"Build the thing end to end","sessionId":"abc"}"#,
        "\n",
        r#"{"type":"last-prompt","lastPrompt":"now polish it","sessionId":"abc"}"#,
        "\n",
    );

    #[test]
    fn prefers_generated_title_and_captures_last_prompt() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        assert_eq!(p.ai_title, "Build the thing end to end");
        assert_eq!(p.title, "build the thing"); // fallback still captured
        assert_eq!(p.last_prompt, "now polish it");
    }

    #[test]
    fn tokens_dedupe_by_message_id_with_last_snapshot_winning() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        // msg_01AAA appears twice (100 then 150): the final snapshot
        // counts once. Plus msg_01BBB's 50.
        assert_eq!(p.output_tokens, 200);
    }

    #[test]
    fn costs_priced_per_model_with_cache_rates() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        // msg_01AAA (Opus, $5/$25, final snapshot wins):
        //   10 in + 150 out + 1000 cache-write (1.25×) + 10000 cache-read (0.1×)
        //   = (50 + 3750 + 6250 + 5000) / 1e6 = 0.01505
        // msg_01BBB (Fable, $10/$50): (5·10 + 50·50) / 1e6 = 0.00255
        assert!((p.cost_usd - 0.01760).abs() < 1e-9, "got {}", p.cost_usd);
    }

    #[test]
    fn model_rates_match_by_family_prefix() {
        assert_eq!(model_rates("claude-fable-5"), (10.0, 50.0));
        assert_eq!(model_rates("claude-opus-4-8"), (5.0, 25.0));
        assert_eq!(model_rates("claude-sonnet-5"), (3.0, 15.0));
        assert_eq!(model_rates("claude-haiku-4-5-20251001"), (1.0, 5.0));
        // Unknown models price at Opus tier rather than zero.
        assert_eq!(model_rates("claude-next-99"), (5.0, 25.0));
    }

    #[test]
    fn models_counted_per_deduped_message() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        // msg_01AAA's two copies collapse to one Opus message; msg_01BBB
        // is one Fable message. Equal counts tie-break alphabetically.
        assert_eq!(
            p.models,
            vec![
                ("claude-fable-5".to_string(), 1),
                ("claude-opus-4-8".to_string(), 1),
            ],
        );
    }

    #[test]
    fn tool_calls_dedupe_by_tool_use_id() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        // toolu_1 appears on both copies of msg_01AAA — one Bash call.
        assert_eq!(
            p.tool_counts,
            vec![("Bash".to_string(), 1), ("Edit".to_string(), 1)],
        );
    }

    #[test]
    fn duration_spans_first_to_last_timestamp() {
        let p = parse_transcript_text(RICH_TRANSCRIPT);
        // 05:58:14 → 06:58:14.
        assert_eq!(p.duration_secs, 3600);
    }

    #[test]
    fn iso_timestamps_parse_to_epoch_seconds() {
        // 2026-07-06 is 20 640 days after the epoch.
        assert_eq!(
            parse_iso_secs("2026-07-06T00:00:00"),
            Some(20_640 * 86_400),
        );
        assert_eq!(parse_iso_secs("1970-01-01T00:00:01"), Some(1));
        assert_eq!(parse_iso_secs("not a timestamp!!!!"), None);
    }

    #[test]
    fn array_content_records_use_their_text_block() {
        let text = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"array-form prompt"}]}}"#,
            "\n",
        );
        let p = parse_transcript_text(text);
        assert_eq!(p.title, "array-form prompt");
    }
}
