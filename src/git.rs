use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const SUBPATH: &str = "Documents/code";
const MAX_REPOS: usize = 30;
const MAX_BRANCHES_PER_REPO: usize = 8;

pub struct GitTree {
    pub root_display: String,
    pub total_repos: usize,
    pub repos: Vec<RepoSummary>,
    pub scanned_at: Instant,
}

pub struct RepoSummary {
    pub name: String,
    pub most_recent_commit: Option<u64>,
    pub branches: Vec<BranchInfo>,
    pub is_dirty: bool,
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

fn scan() -> Option<GitTree> {
    let root = home()?.join(SUBPATH);
    if !root.is_dir() {
        return None;
    }

    let mut repos: Vec<RepoSummary> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join(".git").exists())
        .filter_map(|e| summarize_repo(&e.path()))
        .collect();

    repos.sort_by(|a, b| b.most_recent_commit.cmp(&a.most_recent_commit));
    let total_repos = repos.len();
    repos.truncate(MAX_REPOS);

    Some(GitTree {
        root_display: format!("~/{SUBPATH}"),
        total_repos,
        repos,
        scanned_at: Instant::now(),
    })
}

fn summarize_repo(path: &Path) -> Option<RepoSummary> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let current = git_current_branch(path);
    let mut branches = git_branches(path);
    for b in &mut branches {
        b.is_current = current.as_deref() == Some(&b.name);
    }
    let most_recent_commit = branches.iter().filter_map(|b| b.last_commit).max();
    let is_dirty = git_is_dirty(path);
    branches.truncate(MAX_BRANCHES_PER_REPO);
    Some(RepoSummary {
        name,
        most_recent_commit,
        branches,
        is_dirty,
    })
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

fn git_is_dirty(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}
