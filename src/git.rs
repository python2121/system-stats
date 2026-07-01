use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const SUBPATH: &str = "Documents/code";
const MAX_REPOS: usize = 30;
const MAX_BRANCHES_PER_REPO: usize = 8;
const HEATMAP_LOOKBACK_DAYS: u32 = 400;
const GRAPH_MAX_ROWS: usize = 200;
const MAX_RECENT_COMMITS: usize = 200;

pub struct GitTree {
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
}

pub struct RepoSummary {
    pub name: String,
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

pub fn spawn_scanner() -> Receiver<GitTree> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            if let Some(tree) = scan() {
                if tx.send(tree).is_err() {
                    // UI dropped the receiver; we're done.
                    return;
                }
            }
            thread::sleep(SCAN_INTERVAL);
        }
    });
    rx
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn repo_dir(name: &str) -> Option<PathBuf> {
    Some(home()?.join(SUBPATH).join(name))
}

fn scan() -> Option<GitTree> {
    let root = home()?.join(SUBPATH);
    if !root.is_dir() {
        return None;
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
        root_display: format!("~/{SUBPATH}"),
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
    let out = match Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)|%(committerdate:unix)|%(upstream:track)|%(subject)",
            "refs/heads/",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // splitn(4) so a '|' inside the subject doesn't break parsing.
            let mut parts = line.splitn(4, '|');
            let name = parts.next()?.to_string();
            let ts: Option<u64> = parts.next()?.parse().ok();
            let track = parts.next().unwrap_or("");
            let subject = parts.next().unwrap_or("").to_string();
            let (ahead, behind) = parse_track(track);
            Some(BranchInfo {
                name,
                is_current: false,
                ahead,
                behind,
                last_commit: ts,
                last_message: subject,
            })
        })
        .collect()
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
            "--format=%x00%H%x1f%ct%x1f%an%x1f%s%x1f%D",
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
        };
    };

    let mut parts = payload.split('\x1f');
    let sha = parts.next().unwrap_or("").to_string();
    let ts: Option<u64> = parts.next().and_then(|s| s.parse().ok());
    let author = parts.next().unwrap_or("").to_string();
    let subject = parts.next().unwrap_or("").to_string();
    let refs_raw = parts.next().unwrap_or("");

    let refs: Vec<String> = refs_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    GraphRow {
        prefix: prefix.to_string(),
        sha: Some(sha),
        timestamp: ts,
        author,
        subject,
        refs,
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
