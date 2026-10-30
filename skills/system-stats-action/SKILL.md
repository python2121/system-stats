---
name: system-stats-action
description: Register, update, list or remove a custom action (a "build and run"-style launcher) for the current project in the user's system-stats dashboard, using the `system-stats action` CLI. Use when the user asks to add a build/run/test/deploy command to system-stats, to "make a launcher for this project", or to put something in a repo's menu in system stats.
---

# system-stats custom actions

system-stats is the user's ratatui project dashboard. On its Git Status tab,
pressing Enter on a repo opens a menu; custom actions are the user-defined
rows in that menu. Each one is a shell command tied to one repo. This skill
registers them with the CLI instead of editing the config file by hand.

## How an action runs

Know this before writing the command:

- Running the action opens a **new terminal window at the repo directory**
  (Ghostty if that is what the dashboard runs in, else `$TERMINAL`) and types
  the command into a **login shell**. The command starts with the repo as the
  working directory, so relative paths like `./build.sh` work.
- With `--close`, the command is wrapped as `(cmd) && exit`: the window
  closes on success and stays open on failure so the error is readable.
  Use `--close` for fire-and-forget builds that launch a GUI app. Leave it
  off for anything whose output the user wants to read (tests, logs, servers).
- The command is **one line**. No tabs or newlines. Chain steps with `&&`.
- The dashboard re-reads the config when a repo menu opens. No restart.

## The CLI

```sh
system-stats action list [--repo <path>]
system-stats action add --repo <path> --name <name> [--close] --command '<cmd>'
system-stats action add --repo <path> --name <name> [--close] -- <cmd words…>
system-stats action remove --repo <path> --name <name>
system-stats action path
```

Prefer `--command '<cmd>'` with the whole command single-quoted. The `--`
form joins the remaining words with spaces, which means your shell will
interpret `&&`, `|`, `>` unless you quote them.

If `system-stats` is not on PATH, run it from the checkout:
`cargo run --quiet --manifest-path ~/Documents/code/system-stats/Cargo.toml -- action …`

`--repo` can be `.` from inside the project. It must be an existing
directory, and it should be a git repo directly under the dashboard's
watched directory (default `~/Documents/code`) or the menu will not list it.
The CLI warns on stderr when that is the case; relay the warning.

`add` with a name that already exists for the repo **replaces** it, so
re-running an `add` to fix a command is safe. Names are what the user sees
in the menu: short, Title Case, verb first ("Build and Run", "Run Tests",
"Deploy Staging").

## Environment rule: toolchains in containers

On this machine (SteamOS) toolchains often live in distrobox containers,
and the dashboard runs the command on the host side of the terminal. If
the project builds inside a container, wrap the command so it enters that
container, and `cd` explicitly inside it because the container shell
starts in `$HOME`:

```sh
system-stats action add --repo . --name "Build and Run" --close --command \
  "distrobox enter swiftdev -- bash -lc 'cd ~/Documents/code/SwiftInvert && swift build -c release && .build/release/App'"
```

If the dashboard itself runs inside a distrobox and the command must reach
the host, prefix with `distrobox-host-exec`. Check how the project is
actually built (look for a container name in its README, CLAUDE.md, or an
existing action via `system-stats action list --repo .`) rather than
guessing.

## Procedure

1. Work out the exact command by reading the project's build setup. If
   there is a script or Makefile target that already does the thing, call
   that rather than duplicating its steps.
2. Verify the command runs from a fresh shell in the repo directory before
   registering it, if that is cheap. Do not register something you have
   not seen work unless the user asked for it as-is.
3. Register with `system-stats action add …`.
4. Confirm with `system-stats action list --repo .` and show the user the
   resulting row.
