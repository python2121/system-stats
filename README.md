# system-stats

A terminal dashboard for macOS developers. One window with five tabs: your git activity across every project you're working on, your [Claude Code](https://code.claude.com) sessions (with live status and cost estimates), and per-app process, network, and disk/power monitoring — built with [ratatui](https://ratatui.rs).

No daemons, no root, no config beyond picking a directory to watch. Everything is read live from `git`, `nettop`, `ioreg`, `diskutil`, and Claude Code's local session files.

## Screenshots

<!-- Drop images into docs/ and they'll render here. -->

![Git Status tab](docs/screenshot-git.png)
![Claude tab](docs/screenshot-claude.png)
![Disk / Power tab](docs/screenshot-power.png)

## The tabs

### Git Status

Points at a single "watch directory" (e.g. `~/Documents/code`) and scans every repo one level beneath it:

- A GitHub-style commit heatmap of *your* commits (matched by `git config user.email`), for all repos combined or the selected one
- Per-repo tree: branches with ahead/behind counts, current-branch marker, uncommitted-changes flag, and fork drift against an `upstream` remote
- A colored, lane-styled commit graph for the selected repo, plus a recent-commits feed across all repos
- **Press Enter on a repo** for actions: open a terminal there, start a new Claude session, or **Inspect git** — a full-screen commit graph with per-commit sha, diff stats (+added −removed, files touched), absolute date, and author; Esc goes back

### Claude

Maps each project under the watch directory to its Claude Code session history (`~/.claude/projects/`):

- Every session with its AI-generated title, prompt count, git branch, and age; **live sessions** show a green busy/idle badge and the prompt they're currently working on
- Per-session stats: wall-clock duration, output tokens, which model(s) ran it, a tool-mix profile (`59 Edit · 58 Bash` reads as a build session; `15 WebSearch` as research), and an **API-equivalent cost estimate** (`~$23`) — notional if you're on a subscription plan, computed per message at published API rates
- **Press Enter on an inactive session to resume it**: a new window opens in the terminal app you're currently using (Ghostty, iTerm2, or Terminal.app — detected automatically at launch), cd'd into the project, running `claude --resume`

### Processes

Per-application CPU and memory, with helper processes folded into their parent app the way Activity Monitor does (all the Chrome helpers are just "Google Chrome"). System-wide CPU/memory charts, per-app sparklines, and a drill-down listing every member process.

### Network

Per-application bandwidth from `nettop`, aggregated the same way. Download/upload charts, per-app share bars and sparklines, and a drill-down showing every remote host the app is talking to — reverse-DNS resolved, with connection counts and per-host rates.

### Disk / Power

Disk read/write throughput, IOPS, and latency from cumulative driver counters; mounted volumes with fill gauges (plus unmounted partitions, e.g. a Linux dual-boot); and battery telemetry straight from the SMC — a signed watts flow chart (amber draining, green charging), charge and health gauges, cycle count, per-cell voltages, temperatures, and the negotiated USB-PD contract.

## Requirements

- **macOS** — the network, disk, and power tabs shell out to `nettop`, `ioreg`, and `diskutil`
- **git** on your PATH
- **Rust toolchain** to build ([rustup.rs](https://rustup.rs))
- Optional: **Claude Code** for the Claude tab (it degrades to an empty tab without it); resuming sessions in Ghostty requires Ghostty ≥ 1.3

## Build and run

```sh
cargo build --release
./target/release/system-stats
```

On first launch you'll be asked for the directory to watch — the folder that contains your project checkouts. Everything else is automatic.

## Keys

| Key | Where | Action |
|---|---|---|
| `←` `→` | Tab bar | Switch tabs |
| `↓` / `j` | Tab bar | Drop into the tab's list |
| `↑` / `k` | Top of a list | Back up to the tab bar |
| `↑` `↓` / `k` `j` | Lists | Move the selection |
| `Enter` / `→` / `l` | Lists | Open detail view for the selected item |
| `Enter` | Claude detail | Resume the selected session in your terminal |
| `←` / `h` / `Esc` | Detail views | Back to the list |
| `Esc` | Tab bar | Open the menu (Settings, Exit) |
| `q` / `Ctrl-C` | Anywhere | Quit |

Vim keys (`h j k l`) work wherever arrows do.

## Configuration

A single, human-editable file at `~/.config/system-stats/config` (or `$XDG_CONFIG_HOME/system-stats/config`):

```ini
# system-stats config
watch_dir=/Users/you/Documents/code
```

Change it from inside the app via `Esc → Settings → Watch directory`, or edit the file directly. The terminal app used for resuming Claude sessions is *not* configured — it's detected from `TERM_PROGRAM` at every launch, so it follows you between terminal apps (falling back to Terminal.app under tmux or editor-embedded terminals).

## Notes and caveats

- **Cost estimates are estimates.** Each assistant message is priced by the model that produced it, at API rates current as of mid-2026 (cache writes at 1.25× input assuming 5-minute TTL, reads at 0.1×). Forked sessions double-count their shared history. For subscription users the number is "what this usage would have cost at API rates", not a bill.
- **Claude Code's session files are an internal format**, not a documented API. Parsing is defensive — if a future version changes field names, session rows degrade (titles/counts go blank) rather than break.
- The network tab deliberately runs `nettop` as a one-shot per sample and diffs counters itself; nettop's own logging mode busy-spins at ~140% CPU.
