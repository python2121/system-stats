use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "system-stats";
const CONFIG_FILENAME: &str = "config";

// The terminal emulator the app is running inside. Detected fresh at every
// launch (never persisted) so it follows the user across terminal apps —
// someone who uses Ghostty at home and iTerm at work always gets the one
// they're in right now.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TerminalApp {
    Ghostty,
    Iterm2,
    TerminalApp,
}

impl TerminalApp {
    pub fn label(self) -> &'static str {
        match self {
            TerminalApp::Ghostty => "Ghostty",
            TerminalApp::Iterm2 => "iTerm2",
            TerminalApp::TerminalApp => "Terminal.app",
        }
    }
}

// Best-effort detection of the terminal the app is running inside, via
// the TERM_PROGRAM variable macOS terminals set for their children.
// None when unrecognized — e.g. under tmux (which masks the outer
// terminal as "tmux") or an editor-embedded terminal ("vscode").
pub fn detect_terminal() -> Option<TerminalApp> {
    let tp = std::env::var("TERM_PROGRAM").ok()?;
    if tp.eq_ignore_ascii_case("ghostty") {
        Some(TerminalApp::Ghostty)
    } else if tp == "iTerm.app" {
        Some(TerminalApp::Iterm2)
    } else if tp == "Apple_Terminal" {
        Some(TerminalApp::TerminalApp)
    } else {
        None
    }
}

// A user-defined entry in a repo's action menu — e.g. name "build and run",
// command "./build.sh && open App.app". Keyed by the repo's absolute path so
// each repo gets its own list.
#[derive(Clone)]
pub struct CustomAction {
    pub repo_path: PathBuf,
    pub name: String,
    pub command: String,
    // Close the spawned terminal window once the command succeeds (the
    // runner appends `&& exit`). On failure the window stays open so the
    // error output is readable.
    pub close_on_exit: bool,
}

// Every persisted setting lives here. Add a field + a case in serialize()
// and parse() to grow the schema; unknown keys are ignored on load so old
// binaries survive a newer config file.
#[derive(Clone)]
pub struct Config {
    pub watch_dir: PathBuf,
    pub custom_actions: Vec<CustomAction>,
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
        let mut out = format!(
            "# system-stats config\nwatch_dir={}\n",
            self.watch_dir.display()
        );
        // Tab-separated because the config is line-based and tabs can't be
        // typed into the single-line editor — so path, name, and command are
        // free to contain anything else (spaces, `=`, quotes, …). The
        // replace() is belt-and-braces against a value acquiring a tab or
        // newline some other way, which would corrupt the line.
        // The close flag rides as an optional trailing field so lines from
        // before it existed (and lines for keep-open actions) look the same.
        for a in &self.custom_actions {
            out.push_str(&format!(
                "custom_action={}\t{}\t{}{}\n",
                a.repo_path.display(),
                a.name.replace(['\t', '\n'], " "),
                a.command.replace(['\t', '\n'], " "),
                if a.close_on_exit { "\tclose" } else { "" },
            ));
        }
        out
    }

    // This repo's custom actions, in the order they were added (= the order
    // they appear in the config file). Borrowed straight out of the config.
    pub fn actions_for<'a>(&'a self, repo_path: &'a Path) -> impl Iterator<Item = &'a CustomAction> {
        self.custom_actions
            .iter()
            .filter(move |a| a.repo_path == repo_path)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { watch_dir: default_watch_dir(), custom_actions: Vec::new() }
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
        // A `terminal=` key from earlier versions falls through to the
        // unknown-key arm and is ignored; the value is detected at launch now.
        match key.trim() {
            "watch_dir" => cfg.watch_dir = PathBuf::from(value.trim()),
            // path \t name \t command [\t close] — a malformed line
            // (hand-edited config) is skipped rather than half-loaded.
            "custom_action" => {
                let mut parts = value.splitn(4, '\t');
                if let (Some(p), Some(n), Some(c)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    let close_on_exit =
                        parts.next().map(str::trim) == Some("close");
                    let (p, n, c) = (p.trim(), n.trim(), c.trim());
                    if !p.is_empty() && !n.is_empty() && !c.is_empty() {
                        cfg.custom_actions.push(CustomAction {
                            repo_path: PathBuf::from(p),
                            name: n.to_string(),
                            command: c.to_string(),
                            close_on_exit,
                        });
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_actions_round_trip() {
        let cfg = Config {
            watch_dir: PathBuf::from("/tmp/code"),
            custom_actions: vec![
                CustomAction {
                    repo_path: PathBuf::from("/tmp/code/my app"),
                    name: "build and run".to_string(),
                    command: "./build.sh && open App.app".to_string(),
                    close_on_exit: true,
                },
                CustomAction {
                    repo_path: PathBuf::from("/tmp/code/other"),
                    name: "deploy".to_string(),
                    command: "make deploy ENV=prod".to_string(),
                    close_on_exit: false,
                },
            ],
        };
        let parsed = parse(&cfg.serialize());
        assert_eq!(parsed.watch_dir, cfg.watch_dir);
        assert_eq!(parsed.custom_actions.len(), 2);
        for (a, b) in parsed.custom_actions.iter().zip(&cfg.custom_actions) {
            assert_eq!(a.repo_path, b.repo_path);
            assert_eq!(a.name, b.name);
            assert_eq!(a.command, b.command);
            assert_eq!(a.close_on_exit, b.close_on_exit);
        }
    }

    #[test]
    fn malformed_custom_action_lines_are_skipped() {
        let text = "watch_dir=/tmp/code\n\
                    custom_action=/tmp/code/app\tonly a name\n\
                    custom_action=no tabs at all\n\
                    custom_action=/tmp/code/app\tok\techo hi\n";
        let cfg = parse(text);
        assert_eq!(cfg.custom_actions.len(), 1);
        assert_eq!(cfg.custom_actions[0].name, "ok");
        assert_eq!(cfg.custom_actions[0].command, "echo hi");
        // A pre-flag three-field line loads as a keep-open action.
        assert!(!cfg.custom_actions[0].close_on_exit);
    }

    #[test]
    fn actions_for_filters_by_repo() {
        let mk = |repo: &str, name: &str| CustomAction {
            repo_path: PathBuf::from(repo),
            name: name.to_string(),
            command: "true".to_string(),
            close_on_exit: false,
        };
        let cfg = Config {
            watch_dir: PathBuf::from("/tmp"),
            custom_actions: vec![mk("/a", "one"), mk("/b", "two"), mk("/a", "three")],
        };
        let names: Vec<&str> = cfg
            .actions_for(Path::new("/a"))
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["one", "three"]);
    }
}
