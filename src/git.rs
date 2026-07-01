use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config;

const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const MAX_REPOS: usize = 30;
const MAX_BRANCHES_PER_REPO: usize = 8;
const HEATMAP_LOOKBACK_DAYS: u32 = 400;
const GRAPH_MAX_ROWS: usize = 200;
const MAX_RECENT_COMMITS: usize = 200;

pub struct GitTree {
    // The absolute path this tree was scanned from. Kept so the UI can drop
    // any tree that lands after the user changed the watch dir, avoiding a
    // one-frame flash of stale results.
    pub root: PathBuf,
    pub root_display: String,
    pub total_repos: usize,
    pub repos: Vec<RepoSummary>,
    // Flat list of the author's recent commits across every repo, newest first.
    // Drives the "overall" view shown when no repo is selected.
    pub recent_commits: Vec<RecentCommit>,
    // Aggregate per-day commit count across all repos, for the overall heatmap.
    pub commit_days: HashMap<i64, u32>,
    pub scanned_at: Instant,
}

pub struct RecentCommit {
    pub repo: String,
    pub timestamp: u64,
    pub subject: String,
}

// One row of the commit graph for a single repo.
// `prefix` is the raw graph drawing from `git log --graph` (`* | \ /` etc).
// If `sha` is None, this is a "connector" row with no commit on it.
pub struct GraphRow {
    pub prefix: String,
    pub sha: Option<String>,
    pub timestamp: Option<u64>,
    pub author: String,
    pub subject: String,
    pub refs: Vec<String>,
    // True when the commit has >1 parent — drives the ring-node glyph
    // in the graph renderer.
    pub is_merge: bool,
}

pub struct RepoSummary {
    pub name: String,
    // Absolute path to the repo — used by the graph fetcher so we don't have
    // to re-derive it from name + a hardcoded root.
    pub path: PathBuf,
    pub most_recent_commit: Option<u64>,
    pub branches: Vec<BranchInfo>,
    pub is_dirty: bool,
    pub upstream_remote: Option<String>,
    pub fork_drift: Option<ForkDrift>,
    // Per-day commit count for the heatmap window. Keys are days-since-epoch.
    pub commit_days: HashMap<i64, u32>,
}

pub struct ForkDrift {
    pub branch: String,
    pub ahead: usize,
    pub behind: usize,
}

pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub ahead: usize,
    pub behind: usize,
    pub last_commit: Option<u64>,
    pub last_message: String,
}

// Owns the scanner thread. The UI holds one and reads new trees from
// `tree_rx`; changing the watched directory is a two-step message:
// swap the Mutex-guarded path, then poke the wake channel so the thread
// stops mid-sleep and rescans immediately.
pub struct Scanner {
    tree_rx: Receiver<GitTree>,
    wake_tx: Sender<()>,
    root: Arc<Mutex<PathBuf>>,
}

impl Scanner {
    pub fn try_recv(&self) -> Result<GitTree, mpsc::TryRecvError> {
        self.tree_rx.try_recv()
    }

    // Switch the watched directory and kick off a rescan right now instead
    // of waiting up to SCAN_INTERVAL for the next tick.
    pub fn set_root(&self, new_root: PathBuf) {
        if let Ok(mut guard) = self.root.lock() {
            *guard = new_root;
        }
        // Ignore SendError — a full/disconnected channel just means the
        // thread is exiting; either way the settings write already happened.
        let _ = self.wake_tx.send(());
    }
}

pub fn spawn_scanner(initial_root: PathBuf) -> Scanner {
    let (tree_tx, tree_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let root = Arc::new(Mutex::new(initial_root));
    let thread_root = Arc::clone(&root);
    thread::spawn(move || {
        loop {
            let current = thread_root.lock().ok().map(|g| g.clone());
            if let Some(root) = current {
                if let Some(tree) = scan(&root) {
                    if tree_tx.send(tree).is_err() {
                        // UI dropped the receiver; we're done.
                        return;
                    }
                }
            }
            match wake_rx.recv_timeout(SCAN_INTERVAL) {
                Ok(()) => {
                    // Drain any additional wakes so a burst of set_root
                    // calls collapses into a single rescan.
                    while wake_rx.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    Scanner { tree_rx, wake_tx, root }
}

fn scan(root: &Path) -> Option<GitTree> {
    if !root.is_dir() {
        // Still emit a tree so the UI can render an "empty" state
        // rather than sitting on "scanning …" forever.
        return Some(GitTree {
            root: root.to_path_buf(),
            root_display: config::display_path(root),
            total_repos: 0,
            repos: Vec::new(),
            recent_commits: Vec::new(),
            commit_days: HashMap::new(),
            scanned_at: Instant::now(),
        });
    }

    let repo_paths: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join(".git").exists())
        .collect();

    let author = user_email();
    let mut recent_commits: Vec<RecentCommit> = Vec::new();
    let mut agg_days: HashMap<i64, u32> = HashMap::new();
    let mut repos: Vec<RepoSummary> = Vec::new();
    for path in &repo_paths {
        let Some((summary, commits)) = summarize_repo(path, author.as_deref()) else {
            continue;
        };
        for (day, count) in &summary.commit_days {
            *agg_days.entry(*day).or_insert(0) += count;
        }
        for (ts, subject) in commits {
            recent_commits.push(RecentCommit {
                repo: summary.name.clone(),
                timestamp: ts,
                subject,
            });
        }
        repos.push(summary);
    }

    recent_commits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    recent_commits.truncate(MAX_RECENT_COMMITS);

    repos.sort_by(|a, b| b.most_recent_commit.cmp(&a.most_recent_commit));
    let total_repos = repos.len();
    repos.truncate(MAX_REPOS);

    Some(GitTree {
        root: root.to_path_buf(),
        root_display: config::display_path(root),
        total_repos,
        repos,
        recent_commits,
        commit_days: agg_days,
        scanned_at: Instant::now(),
    })
}

fn user_email() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if email.is_empty() { None } else { Some(email) }
}

fn commits_in_window(path: &Path, author: Option<&str>) -> Vec<(u64, String)> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args([
        "log",
        "--all",
        "--no-merges",
        "--format=%ct|%s",
        &format!("--since={HEATMAP_LOOKBACK_DAYS}.days.ago"),
    ]);
    if let Some(email) = author {
        cmd.arg(format!("--author={email}"));
    }

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // splitn(2) so a '|' inside the subject doesn't break parsing.
            let mut parts = line.splitn(2, '|');
            let ts: u64 = parts.next()?.parse().ok()?;
            let subject = parts.next().unwrap_or("").to_string();
            Some((ts, subject))
        })
        .collect()
}

fn summarize_repo(
    path: &Path,
    author: Option<&str>,
) -> Option<(RepoSummary, Vec<(u64, String)>)> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let current = git_current_branch(path);
    let mut branches = git_branches(path);
    for b in &mut branches {
        b.is_current = current.as_deref() == Some(&b.name);
    }
    let most_recent_commit = branches.iter().filter_map(|b| b.last_commit).max();
    let is_dirty = git_is_dirty(path);
    let upstream_url = git_upstream_remote_url(path);
    let upstream_remote = upstream_url.as_deref().map(shorten_remote_url);
    let fork_drift = if upstream_url.is_some() {
        git_fork_drift(path)
    } else {
        None
    };

    let commits = commits_in_window(path, author);
    let mut commit_days: HashMap<i64, u32> = HashMap::new();
    for (timestamp, _subject) in &commits {
        let day = (*timestamp as i64) / 86_400;
        *commit_days.entry(day).or_insert(0) += 1;
    }

    branches.truncate(MAX_BRANCHES_PER_REPO);
    let summary = RepoSummary {
        name,
        path: path.to_path_buf(),
        most_recent_commit,
        branches,
        is_dirty,
        upstream_remote,
        fork_drift,
        commit_days,
    };
    Some((summary, commits))
}

fn git_upstream_remote_url(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "upstream"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// Turn a remote URL into "owner/repo" — handles both SSH (`git@host:owner/repo.git`)
// and HTTPS (`https://host/owner/repo.git`) shapes without pulling in a URL crate.
fn shorten_remote_url(url: &str) -> String {
    let url = url.trim().trim_end_matches(".git");
    let mut parts = url.rsplit('/').filter(|p| !p.is_empty());
    let last = parts.next();
    let second_last = parts.next();
    match (second_last, last) {
        (Some(s), Some(l)) => {
            let owner = s.rsplit(':').next().unwrap_or(s);
            format!("{owner}/{l}")
        }
        _ => url.to_string(),
    }
}

fn git_fork_drift(path: &Path) -> Option<ForkDrift> {
    // A remote named "upstream" is the conventional pointer to the original
    // project when you've forked something. If it isn't present, nothing to do.
    let exists = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "upstream"])
        .output()
        .ok()?;
    if !exists.status.success() {
        return None;
    }

    let branch = upstream_default_branch(path)?;

    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-list", "--left-right", "--count"])
        .arg(format!("{branch}...upstream/{branch}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().split_whitespace();
    let ahead: usize = parts.next()?.parse().ok()?;
    let behind: usize = parts.next()?.parse().ok()?;

    Some(ForkDrift {
        branch,
        ahead,
        behind,
    })
}

fn upstream_default_branch(path: &Path) -> Option<String> {
    // Preferred: ask git what upstream/HEAD points at. Only set if the user
    // (or a tool) has explicitly tracked it — `git remote set-head upstream -a`.
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--short", "refs/remotes/upstream/HEAD"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = s.strip_prefix("upstream/") {
                return Some(name.to_string());
            }
        }
    }

    // Fallback: probe the two near-universal default names.
    for candidate in ["main", "master"] {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("upstream/{candidate}"))
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                return Some(candidate.to_string());
            }
        }
    }

    None
}

fn git_current_branch(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_branches(path: &Path) -> Vec<BranchInfo> {
    // Ask for local heads AND origin's remote refs in one shot. Two
    // patterns to `for-each-ref` come back in one committerdate-sorted
    // stream, saving a fork/exec per repo. We deliberately don't include
    // other remotes (e.g. `upstream` on forked repos) — those aren't the
    // user's branches, they're the parent project's.
    let out = match Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)|%(committerdate:unix)|%(upstream:track)|%(subject)",
            "refs/heads/",
            "refs/remotes/origin/",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut locals: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut all: Vec<BranchInfo> = Vec::new();
    for line in text.lines() {
        // splitn(4) so a '|' inside the subject doesn't break parsing.
        let mut parts = line.splitn(4, '|');
        let Some(name) = parts.next() else { continue };
        // `origin/HEAD` is a symbolic ref pointing at (typically) `origin/main`.
        // Including it would duplicate the branch it points at.
        if name == "origin/HEAD" {
            continue;
        }
        let name = name.to_string();
        let Some(ts_str) = parts.next() else { continue };
        let ts: Option<u64> = ts_str.parse().ok();
        let track = parts.next().unwrap_or("");
        let subject = parts.next().unwrap_or("").to_string();
        let (ahead, behind) = parse_track(track);
        if !name.starts_with("origin/") {
            locals.insert(name.clone());
        }
        all.push(BranchInfo {
            name,
            is_current: false,
            ahead,
            behind,
            last_commit: ts,
            last_message: subject,
        });
    }

    // Drop remote branches that already have a local counterpart with the
    // same short name — the local BranchInfo already carries the ahead/behind
    // tracking info, so keeping the remote copy would be redundant. Retain
    // preserves the sorted-by-committerdate order from for-each-ref.
    all.retain(|b| match b.name.strip_prefix("origin/") {
        Some(short) => !locals.contains(short),
        None => true,
    });
    all
}

// git's %(upstream:track) values: "", "[gone]", "[ahead N]", "[behind N]",
// "[ahead N, behind N]". Pull out the two numbers without a regex dep.
fn parse_track(s: &str) -> (usize, usize) {
    let mut ahead = 0;
    let mut behind = 0;
    let mut tokens = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty());
    while let Some(tok) = tokens.next() {
        match tok {
            "ahead" => {
                if let Some(n) = tokens.next().and_then(|t| t.parse().ok()) {
                    ahead = n;
                }
            }
            "behind" => {
                if let Some(n) = tokens.next().and_then(|t| t.parse().ok()) {
                    behind = n;
                }
            }
            _ => {}
        }
    }
    (ahead, behind)
}

// Fetch a graph view for a single repo. Runs synchronously — callers should
// cache and only invoke when the selection changes.
pub fn graph(repo_dir: &Path) -> Vec<GraphRow> {
    // `\x1f` (unit separator) is used between fields so subjects / refs can
    // contain anything printable without breaking parsing.
    let out = match Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args([
            "log",
            "--all",
            "--graph",
            "--date-order",
            "--decorate=short",
            "--format=%x00%H%x1f%ct%x1f%an%x1f%s%x1f%D%x1f%P",
            &format!("-n{GRAPH_MAX_ROWS}"),
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().map(parse_graph_line).collect()
}

fn parse_graph_line(line: &str) -> GraphRow {
    // Lines without a NUL marker are pure graph connectors (`|\`, `|/`, etc).
    let Some((prefix, payload)) = line.split_once('\x00') else {
        return GraphRow {
            prefix: line.to_string(),
            sha: None,
            timestamp: None,
            author: String::new(),
            subject: String::new(),
            refs: Vec::new(),
            is_merge: false,
        };
    };

    let mut parts = payload.split('\x1f');
    let sha = parts.next().unwrap_or("").to_string();
    let ts: Option<u64> = parts.next().and_then(|s| s.parse().ok());
    let author = parts.next().unwrap_or("").to_string();
    let subject = parts.next().unwrap_or("").to_string();
    let refs_raw = parts.next().unwrap_or("");
    let parents_raw = parts.next().unwrap_or("");

    let refs: Vec<String> = refs_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let is_merge = parents_raw.split_whitespace().count() > 1;

    GraphRow {
        prefix: prefix.to_string(),
        sha: Some(sha),
        timestamp: ts,
        author,
        subject,
        refs,
        is_merge,
    }
}

fn git_is_dirty(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}
