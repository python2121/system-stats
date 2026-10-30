// Non-interactive `system-stats action …` subcommands. They edit the same
// config file the TUI uses so an agent (or a shell script) can register a
// repo's custom actions without driving the menus. The running TUI picks
// the change up next time its repo menu opens.
//
// Argument parsing is by hand: three subcommands and five flags aren't
// worth a dependency, and the TUI binary stays dependency-light.

use std::path::{Component, Path, PathBuf};

use crate::config::{self, Config, CustomAction};

pub const USAGE: &str = "\
system-stats — project dashboard TUI

USAGE
  system-stats                        start the TUI
  system-stats action list [--repo <path>]
  system-stats action add  --repo <path> --name <name> [--close] -- <command…>
  system-stats action add  --repo <path> --name <name> [--close] --command <command>
  system-stats action remove --repo <path> --name <name>
  system-stats action path            print the config file path

A custom action is a shell command shown in the repo's Enter menu on the
Git Status tab. Running it opens a new terminal window at the repo
directory and types the command into a login shell. With --close the
window closes once the command succeeds (it is run as `(cmd) && exit`);
on failure the window stays open so the error is readable.

`add` with a name that already exists for that repo replaces that action
in place. <path> may be relative or use ~; it must be an existing directory.
Names and commands cannot contain tabs or newlines.";

#[derive(Debug, PartialEq, Eq)]
pub enum ActionCmd {
    List { repo: Option<PathBuf> },
    Add { repo: PathBuf, name: String, command: String, close: bool },
    Remove { repo: PathBuf, name: String },
    Path,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Help,
    Action(ActionCmd),
}

// Entry point from main(). Returns the process exit code: 0 ok, 1 the
// request failed (bad path, write error), 2 usage error.
pub fn run(args: &[String]) -> i32 {
    let cmd = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return 2;
        }
    };
    match cmd {
        Cmd::Help => {
            println!("{USAGE}");
            0
        }
        Cmd::Action(ActionCmd::Path) => match config::config_path() {
            Some(p) => {
                println!("{}", p.display());
                0
            }
            None => {
                eprintln!("error: HOME is not set");
                1
            }
        },
        Cmd::Action(action) => {
            let mut cfg = Config::load().unwrap_or_default();
            let action = match resolve_repo(action, &cfg.watch_dir) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            };
            let mutates = !matches!(action, ActionCmd::List { .. });
            match apply(&mut cfg, &action) {
                Ok(msg) => {
                    if mutates && let Err(e) = cfg.save() {
                        eprintln!("error: failed to save config: {e}");
                        return 1;
                    }
                    println!("{msg}");
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
    }
}

pub fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => Ok(Cmd::Help),
        Some("action") => parse_action(&args[1..]).map(Cmd::Action),
        Some(other) => Err(format!("unknown command `{other}`")),
    }
}

fn parse_action(args: &[String]) -> Result<ActionCmd, String> {
    let Some(sub) = args.first() else {
        return Err("`action` needs a subcommand: list, add, remove, path".into());
    };
    let mut repo: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut command: Option<String> = None;
    let mut close = false;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--repo" | "--name" | "--command" => {
                let v = args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))?;
                match a {
                    "--repo" => repo = Some(config::expand_tilde(v)),
                    "--name" => name = Some(v.clone()),
                    _ => command = Some(v.clone()),
                }
                i += 2;
            }
            "--close" => {
                close = true;
                i += 1;
            }
            "--" => {
                let rest = &args[i + 1..];
                if rest.is_empty() {
                    return Err("nothing after `--`".into());
                }
                command = Some(rest.join(" "));
                break;
            }
            _ => return Err(format!("unexpected argument `{a}`")),
        }
    }
    let need_repo = || repo.clone().ok_or_else(|| "--repo is required".to_string());
    let need_name = || -> Result<String, String> {
        let n = name.clone().ok_or_else(|| "--name is required".to_string())?;
        check_text("name", &n)?;
        Ok(n)
    };
    match sub.as_str() {
        "list" => Ok(ActionCmd::List { repo }),
        "path" => Ok(ActionCmd::Path),
        "add" => {
            let command = command.ok_or_else(|| {
                "a command is required: `--command <cmd>` or `-- <cmd…>`".to_string()
            })?;
            let command = command.trim().to_string();
            check_text("command", &command)?;
            Ok(ActionCmd::Add { repo: need_repo()?, name: need_name()?.trim().to_string(), command, close })
        }
        "remove" => Ok(ActionCmd::Remove { repo: need_repo()?, name: need_name()?.trim().to_string() }),
        other => Err(format!("unknown action subcommand `{other}`")),
    }
}

// The config is line-based and tab-separated, so these two characters
// would corrupt it. Refuse rather than silently rewrite what was asked for.
fn check_text(what: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if value.contains(['\t', '\n']) {
        return Err(format!("{what} cannot contain tabs or newlines"));
    }
    Ok(())
}

// Turn the user's path into the exact key the TUI will look it up by. The
// scanner lists repos as `watch_dir.join(entry name)` without resolving
// symlinks, so a repo under the watch dir is rewritten onto that prefix
// rather than canonicalized outright — otherwise a symlinked watch dir
// (or a `..`-laden relative path) would produce a key the menu never
// matches. Anything else keeps its lexically-normalized absolute form.
fn resolve_repo(action: ActionCmd, watch_dir: &Path) -> Result<ActionCmd, String> {
    let fix = |p: PathBuf| -> Result<PathBuf, String> {
        let abs = absolutize(&p);
        if !abs.is_dir() {
            return Err(format!("not a directory: {}", abs.display()));
        }
        let key = match (abs.canonicalize(), watch_dir.canonicalize()) {
            (Ok(c), Ok(w)) => match c.strip_prefix(&w) {
                Ok(rel) if !rel.as_os_str().is_empty() => watch_dir.join(rel),
                _ => abs.clone(),
            },
            _ => abs.clone(),
        };
        if key.strip_prefix(watch_dir).is_err() {
            eprintln!(
                "warning: {} is not under the watched directory {}; the repo menu won't list it",
                key.display(),
                watch_dir.display()
            );
        } else if !key.join(".git").exists() {
            eprintln!("warning: {} has no .git directory; the repo menu won't list it", key.display());
        }
        Ok(key)
    };
    Ok(match action {
        ActionCmd::List { repo } => ActionCmd::List { repo: repo.map(fix).transpose()? },
        ActionCmd::Add { repo, name, command, close } => {
            ActionCmd::Add { repo: fix(repo)?, name, command, close }
        }
        ActionCmd::Remove { repo, name } => ActionCmd::Remove { repo: fix(repo)?, name },
        ActionCmd::Path => ActionCmd::Path,
    })
}

// cwd-relative → absolute, with `.` and `..` folded lexically (no symlink
// resolution, no filesystem access).
fn absolutize(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// Pure edit of an in-memory config; the caller saves. Returns the line to
// print on success.
pub fn apply(cfg: &mut Config, action: &ActionCmd) -> Result<String, String> {
    match action {
        ActionCmd::Path => Ok(String::new()),
        ActionCmd::List { repo } => {
            let rows: Vec<&CustomAction> = cfg
                .custom_actions
                .iter()
                .filter(|a| repo.as_ref().is_none_or(|r| &a.repo_path == r))
                .collect();
            if rows.is_empty() {
                return Ok(match repo {
                    Some(r) => format!("no custom actions for {}", config::display_path(r)),
                    None => "no custom actions".to_string(),
                });
            }
            Ok(rows
                .iter()
                .map(|a| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        config::display_path(&a.repo_path),
                        a.name,
                        if a.close_on_exit { "close" } else { "keep-open" },
                        a.command
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ActionCmd::Add { repo, name, command, close } => {
            let new = CustomAction {
                repo_path: repo.clone(),
                name: name.clone(),
                command: command.clone(),
                close_on_exit: *close,
            };
            let existing = cfg
                .custom_actions
                .iter_mut()
                .find(|a| &a.repo_path == repo && &a.name == name);
            let verb = match existing {
                Some(slot) => {
                    *slot = new;
                    "updated"
                }
                None => {
                    cfg.custom_actions.push(new);
                    "added"
                }
            };
            Ok(format!(
                "{verb} action \"{name}\" for {}{}",
                config::display_path(repo),
                if *close { " (window closes on success)" } else { "" }
            ))
        }
        ActionCmd::Remove { repo, name } => {
            let before = cfg.custom_actions.len();
            cfg.custom_actions.retain(|a| !(&a.repo_path == repo && &a.name == name));
            let removed = before - cfg.custom_actions.len();
            if removed == 0 {
                return Err(format!(
                    "no action named \"{name}\" for {}",
                    config::display_path(repo)
                ));
            }
            Ok(format!("removed action \"{name}\" for {}", config::display_path(repo)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn cfg_with(actions: Vec<(&str, &str, &str)>) -> Config {
        Config {
            watch_dir: PathBuf::from("/code"),
            custom_actions: actions
                .into_iter()
                .map(|(r, n, c)| CustomAction {
                    repo_path: PathBuf::from(r),
                    name: n.to_string(),
                    command: c.to_string(),
                    close_on_exit: false,
                })
                .collect(),
            ..Config::default()
        }
    }

    #[test]
    fn no_args_and_help_flags_mean_help() {
        for a in [&[][..], &["help"], &["--help"], &["-h"]] {
            assert_eq!(parse_args(&args(a)), Ok(Cmd::Help));
        }
    }

    #[test]
    fn add_takes_command_after_double_dash_joined_with_spaces() {
        let parsed = parse_args(&args(&[
            "action", "add", "--repo", "/code/app", "--name", "Build", "--close", "--",
            "cargo", "build", "&&", "./run", "--flag",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Cmd::Action(ActionCmd::Add {
                repo: PathBuf::from("/code/app"),
                name: "Build".into(),
                command: "cargo build && ./run --flag".into(),
                close: true,
            })
        );
    }

    #[test]
    fn add_takes_command_flag_too() {
        let parsed = parse_args(&args(&[
            "action", "add", "--repo", "/code/app", "--name", "Build", "--command", "make",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            Cmd::Action(ActionCmd::Add {
                repo: PathBuf::from("/code/app"),
                name: "Build".into(),
                command: "make".into(),
                close: false,
            })
        );
    }

    #[test]
    fn add_requires_repo_name_and_command() {
        let e = parse_args(&args(&["action", "add", "--name", "x", "--", "make"])).unwrap_err();
        assert!(e.contains("--repo"), "{e}");
        let e = parse_args(&args(&["action", "add", "--repo", "/r", "--", "make"])).unwrap_err();
        assert!(e.contains("--name"), "{e}");
        let e = parse_args(&args(&["action", "add", "--repo", "/r", "--name", "x"])).unwrap_err();
        assert!(e.contains("command"), "{e}");
        let e = parse_args(&args(&["action", "add", "--repo", "/r", "--name", "x", "--"])).unwrap_err();
        assert!(e.contains("--"), "{e}");
    }

    #[test]
    fn tabs_and_newlines_are_refused() {
        let e = parse_args(&args(&["action", "add", "--repo", "/r", "--name", "a\tb", "--", "make"]))
            .unwrap_err();
        assert!(e.contains("name"), "{e}");
        let e = parse_args(&args(&["action", "add", "--repo", "/r", "--name", "a", "--", "make\nall"]))
            .unwrap_err();
        assert!(e.contains("command"), "{e}");
    }

    #[test]
    fn unknown_things_are_errors() {
        assert!(parse_args(&args(&["frobnicate"])).is_err());
        assert!(parse_args(&args(&["action"])).is_err());
        assert!(parse_args(&args(&["action", "zap"])).is_err());
        assert!(parse_args(&args(&["action", "list", "--bogus"])).is_err());
        assert!(parse_args(&args(&["action", "list", "--repo"])).is_err());
    }

    #[test]
    fn add_appends_then_replaces_in_place() {
        let mut cfg = cfg_with(vec![("/code/a", "one", "x"), ("/code/b", "two", "y")]);
        let add = |name: &str, command: &str, close: bool| ActionCmd::Add {
            repo: PathBuf::from("/code/a"),
            name: name.into(),
            command: command.into(),
            close,
        };
        let msg = apply(&mut cfg, &add("three", "z", true)).unwrap();
        assert!(msg.starts_with("added"), "{msg}");
        assert_eq!(cfg.custom_actions.len(), 3);
        assert!(cfg.custom_actions[2].close_on_exit);

        let msg = apply(&mut cfg, &add("one", "x2", false)).unwrap();
        assert!(msg.starts_with("updated"), "{msg}");
        assert_eq!(cfg.custom_actions.len(), 3);
        assert_eq!(cfg.custom_actions[0].name, "one");
        assert_eq!(cfg.custom_actions[0].command, "x2");
        // Same name under a different repo is a separate action.
        assert_eq!(cfg.custom_actions[1].command, "y");
    }

    #[test]
    fn remove_deletes_matching_and_errors_on_miss() {
        let mut cfg = cfg_with(vec![("/code/a", "one", "x"), ("/code/b", "one", "y")]);
        let rm = ActionCmd::Remove { repo: PathBuf::from("/code/a"), name: "one".into() };
        apply(&mut cfg, &rm).unwrap();
        assert_eq!(cfg.custom_actions.len(), 1);
        assert_eq!(cfg.custom_actions[0].repo_path, PathBuf::from("/code/b"));
        assert!(apply(&mut cfg, &rm).unwrap_err().contains("no action"));
    }

    #[test]
    fn list_filters_by_repo() {
        let mut cfg = cfg_with(vec![("/code/a", "one", "x"), ("/code/b", "two", "y")]);
        let all = apply(&mut cfg, &ActionCmd::List { repo: None }).unwrap();
        assert_eq!(all.lines().count(), 2);
        let a = apply(&mut cfg, &ActionCmd::List { repo: Some(PathBuf::from("/code/a")) }).unwrap();
        assert_eq!(a.lines().count(), 1);
        assert!(a.contains("one") && a.contains("keep-open"), "{a}");
        let none = apply(&mut cfg, &ActionCmd::List { repo: Some(PathBuf::from("/code/zz")) }).unwrap();
        assert!(none.starts_with("no custom actions"), "{none}");
    }

    #[test]
    fn absolutize_folds_dot_segments_without_touching_disk() {
        assert_eq!(absolutize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(absolutize(Path::new("/a/../../b")), PathBuf::from("/b"));
        let rel = absolutize(Path::new("x/./y"));
        assert!(rel.is_absolute());
        assert!(rel.ends_with("x/y"));
    }

    #[test]
    fn resolve_repo_rewrites_onto_the_watch_dir_prefix() {
        // Watch dir reached through a symlink: the key must use the
        // configured (symlinked) prefix, not the canonical target.
        let base = std::env::temp_dir().join(format!("ss-cli-{}", std::process::id()));
        let real = base.join("real");
        let repo = real.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let link = base.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();

        let via_canonical = real.join("proj");
        let out = resolve_repo(
            ActionCmd::Remove { repo: via_canonical, name: "n".into() },
            &link,
        )
        .unwrap();
        assert_eq!(out, ActionCmd::Remove { repo: link.join("proj"), name: "n".into() });

        let err = resolve_repo(
            ActionCmd::Remove { repo: base.join("missing"), name: "n".into() },
            &link,
        )
        .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");

        std::fs::remove_dir_all(&base).unwrap();
    }
}
