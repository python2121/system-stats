use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "system-stats";
const CONFIG_FILENAME: &str = "config";

// Every persisted setting lives here. Add a field + a case in serialize()
// and parse() to grow the schema; unknown keys are ignored on load so old
// binaries survive a newer config file.
#[derive(Clone)]
pub struct Config {
    pub watch_dir: PathBuf,
}

impl Config {
    pub fn load() -> Option<Self> {
        let path = config_path()?;
        let text = fs::read_to_string(&path).ok()?;
        Some(parse(&text))
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "HOME is not set")
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, self.serialize())
    }

    // Distinguishes "first launch ever" from "config was loaded". Used to
    // decide whether the setup prompt is cancelable.
    pub fn exists() -> bool {
        config_path().map(|p| p.exists()).unwrap_or(false)
    }

    fn serialize(&self) -> String {
        format!(
            "# system-stats config\nwatch_dir={}\n",
            self.watch_dir.display()
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { watch_dir: default_watch_dir() }
    }
}

fn default_watch_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Documents/code"))
        .unwrap_or_else(|| PathBuf::from("."))
}

// Line-based key=value. Blank lines and `#` comments are skipped. Missing
// keys fall back to Default so a partial config doesn't nuke settings that
// were added in a newer version.
fn parse(text: &str) -> Config {
    let mut cfg = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "watch_dir" => cfg.watch_dir = PathBuf::from(value.trim()),
            _ => {}
        }
    }
    cfg
}

// XDG_CONFIG_HOME wins if set to a non-empty value; otherwise standard
// $HOME/.config. Returns None only when HOME itself is unset — rare, but
// callers treat it as "no config to load and no place to save".
pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join(APP_DIR).join(CONFIG_FILENAME));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config").join(APP_DIR).join(CONFIG_FILENAME))
}

// "/Users/andrew/Documents/code" → "~/Documents/code" when the path is
// under $HOME. Used for the pane title and to prefill the settings prompt.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home_pb = PathBuf::from(home);
        if let Ok(rel) = path.strip_prefix(&home_pb) {
            let rel_str = rel.display().to_string();
            return if rel_str.is_empty() {
                "~".to_string()
            } else {
                format!("~/{rel_str}")
            };
        }
    }
    path.display().to_string()
}

// User types `~/foo` → return `$HOME/foo`. Bare `~` maps to `$HOME`.
// Anything else passes through as-is; the caller decides how to absolutize.
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}
