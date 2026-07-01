//! Per-application bandwidth monitor built on top of macOS' `nettop`.
//!
//! We run `nettop -L 1` as a ~10ms one-shot every sample interval and
//! diff its cumulative per-connection counters ourselves. A long-running
//! `nettop -L 0`/`-l 0` would hand us ready-made deltas, but its logging
//! mode busy-spins between samples (~140% CPU sustained, regardless of
//! `-s`) — measured pathological, so we do the delta tracking instead.
//! `-n` keeps hostname resolution off so an `IP → name` rewrite between
//! samples doesn't split a connection across two aggregation keys.
//!
//! Traffic is grouped by *application* first (all `Google Chrome Helper`
//! variants collapse into "Google Chrome", `plugin-container` joins
//! Firefox, and so on) with a per-remote-host breakdown kept inside each
//! app so the UI can show "who is this app talking to".

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
// EMA time constant — the "smoothing over 5–10s" the user asked for.
// Larger = steadier but slower to react.
pub const EMA_TAU_SECS: f64 = 5.0;
// Rolling-history length for the per-app sparkline + peak-based sort.
// 30 samples at 2s cadence = 60s of history in view.
pub const HISTORY_LEN: usize = 30;
// Rolling-history length for the whole-system throughput charts in the
// header. 240 samples at 2s cadence = 8 minutes.
pub const TOTAL_HISTORY_LEN: usize = 240;
// An app with no traffic for this long is removed from state entirely.
pub const REMOVE_AFTER_SECS: u64 = 120;

pub struct NetSample {
    pub interval: Duration,
    // Per-application deltas over this sample interval, keyed by the
    // canonical app name (helpers already folded in).
    pub apps: HashMap<String, AppDelta>,
    // Aggregate across every non-loopback connection, before per-app
    // grouping. Used for the throughput charts in the UI header.
    pub total_in: u64,
    pub total_out: u64,
}

#[derive(Default)]
pub struct AppDelta {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub conn_count: u32,
    // Per-remote-host breakdown within this app.
    pub hosts: HashMap<IpAddr, HostDelta>,
}

#[derive(Default)]
pub struct HostDelta {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub conn_count: u32,
}

pub struct Monitor {
    rx: Receiver<NetSample>,
}

impl Monitor {
    pub fn try_recv(&self) -> Result<NetSample, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

// One complete `-L 1` snapshot: header row, then process rows each
// followed by their connection rows, with cumulative byte counters.
fn run_nettop() -> Option<String> {
    let out = Command::new("nettop")
        .args(["-x", "-n", "-L", "1", "-J", "bytes_in,bytes_out", "-t", "external"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Returns None if nettop couldn't run — caller falls back to showing an
// error state instead of crashing. The probe doubles as the baseline
// sample, so the app/connection inventory shows up immediately (with
// zero rates) rather than after the first interval.
pub fn spawn_monitor() -> Option<Monitor> {
    let first = run_nettop()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut tracker = DeltaTracker::new();
        if tx.send(tracker.build_sample(&first)).is_err() {
            return;
        }
        loop {
            thread::sleep(SAMPLE_INTERVAL);
            // A transient failure (e.g. fork pressure) skips one tick;
            // the smoothing in NetworkState rides over the gap.
            let Some(text) = run_nettop() else { continue };
            if tx.send(tracker.build_sample(&text)).is_err() {
                // Receiver dropped — the UI is going away.
                return;
            }
        }
    });
    Some(Monitor { rx })
}

// Turns consecutive cumulative snapshots into per-interval deltas.
//
// nettop's counters are per-connection and count from the connection's
// birth, so: a connection seen in the previous snapshot contributes
// (current − previous); a connection first seen *after* our first
// snapshot is genuinely new and its whole counter is this interval's
// traffic; everything in the very first snapshot is pre-existing history
// and contributes zero. A counter that went backwards means the 4-tuple
// was reused by a fresh connection — treat like new.
struct DeltaTracker {
    // Cumulative (bytes_in, bytes_out) per connection from the previous
    // snapshot, keyed by "<process label>|<connection descriptor>". The
    // pid-bearing process label keeps two pids of one binary distinct.
    prev: HashMap<String, (u64, u64)>,
    // False only for the first snapshot, whose counters are all history.
    primed: bool,
}

impl DeltaTracker {
    fn new() -> Self {
        Self { prev: HashMap::new(), primed: false }
    }

    fn build_sample(&mut self, text: &str) -> NetSample {
        let mut apps: HashMap<String, AppDelta> = HashMap::new();
        let mut total_in = 0u64;
        let mut total_out = 0u64;
        // Connection rows belong to the most recent process row above them.
        let mut current_app: Option<String> = None;
        let mut current_label = String::new();
        let mut next: HashMap<String, (u64, u64)> = HashMap::new();

        for line in text.lines() {
            let Some(row) = parse_row(line) else { continue };
            match row {
                Row::Process { name, raw } => {
                    current_app = Some(canonical_app(name));
                    current_label = raw.to_string();
                }
                Row::Connection { label, remote_ip, bytes_in, bytes_out } => {
                    if is_ignorable(&remote_ip) {
                        continue;
                    }
                    let key = format!("{current_label}|{label}");
                    let (d_in, d_out) = match self.prev.get(&key) {
                        Some(&(p_in, p_out)) => {
                            if bytes_in < p_in || bytes_out < p_out {
                                // Reused 4-tuple: fresh connection.
                                (bytes_in, bytes_out)
                            } else {
                                (bytes_in - p_in, bytes_out - p_out)
                            }
                        }
                        None if self.primed => (bytes_in, bytes_out),
                        None => (0, 0),
                    };
                    next.insert(key, (bytes_in, bytes_out));

                    let app_name = current_app
                        .clone()
                        .unwrap_or_else(|| "(unknown)".to_string());
                    let app = apps.entry(app_name).or_default();
                    let host = app.hosts.entry(remote_ip).or_default();
                    // Idle connections still count toward conn totals so
                    // the UI can show "34 conns" even when most are quiet.
                    app.conn_count += 1;
                    host.conn_count += 1;
                    if d_in == 0 && d_out == 0 {
                        continue;
                    }
                    app.bytes_in += d_in;
                    app.bytes_out += d_out;
                    host.bytes_in += d_in;
                    host.bytes_out += d_out;
                    total_in += d_in;
                    total_out += d_out;
                }
            }
        }

        // Connections that vanished this snapshot fall out of `prev`
        // naturally; a later same-4-tuple connection is then "new".
        self.prev = next;
        self.primed = true;
        NetSample { interval: SAMPLE_INTERVAL, apps, total_in, total_out }
    }
}

enum Row<'a> {
    Process { name: &'a str, raw: &'a str },
    Connection { label: &'a str, remote_ip: IpAddr, bytes_in: u64, bytes_out: u64 },
}

fn parse_row(line: &str) -> Option<Row<'_>> {
    // Row shape: "<label>,<bytes_in>,<bytes_out>,"
    // splitn(3) so a label containing commas stays intact.
    let mut parts = line.splitn(3, ',');
    let label = parts.next()?;
    if label.is_empty() {
        // Header rows are handled by the caller; nettop occasionally emits
        // blank rows we can skip.
        return None;
    }
    let bytes_in_str = parts.next()?;
    let rest = parts.next()?;
    let bytes_out_str = rest.trim_end_matches(',');

    if label.contains("<->") {
        // Connection row. Fields with no traffic and no state are printed
        // as empty — parse those as zero.
        let bytes_in = bytes_in_str.parse::<u64>().unwrap_or(0);
        let bytes_out = bytes_out_str.parse::<u64>().unwrap_or(0);
        let remote_ip = parse_remote_ip(label)?;
        Some(Row::Connection { label, remote_ip, bytes_in, bytes_out })
    } else {
        // Process row: "<name>.<pid>". Strip the ".pid" suffix so multiple
        // pids of the same binary aggregate; keep the raw pid-bearing
        // label too — the delta tracker keys connections with it.
        let name = strip_pid(label);
        Some(Row::Process { name, raw: label })
    }
}

// "firefox.935" → "firefox"; "Code Helper.3745" → "Code Helper".
// Falls back to the whole label if the last dot doesn't precede an all-digit
// tail (defensive against unusual process names).
fn strip_pid(label: &str) -> &str {
    match label.rsplit_once('.') {
        Some((name, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => name,
        _ => label,
    }
}

// Fold helper/child processes into the application the user would name.
// Chromium-family apps (Chrome, Edge, Brave, Slack, Discord, VS Code, …)
// all spawn "<App> Helper (Renderer/GPU/…)" children — the generic
// " Helper" strip catches every one of them. Firefox and Safari use their
// own multi-process schemes, handled explicitly.
pub fn canonical_app(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower == "firefox" || lower == "plugin-container" {
        return "Firefox".to_string();
    }
    if lower == "safari" || lower.starts_with("com.apple.webkit") {
        return "Safari".to_string();
    }
    if let Some(idx) = name.find(" Helper") {
        return name[..idx].to_string();
    }
    // Claude Code's native install names its executable after the bare
    // version ("~/.local/share/claude/versions/2.1.198"), so nettop
    // reports the process as "2.1.198". Nothing else names binaries as
    // pure version strings, so claim those.
    let mut parts = name.split('.');
    if parts.clone().count() >= 2
        && parts.all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return "Claude Code".to_string();
    }
    name.to_string()
}

// Pull the remote IP out of a connection descriptor like:
//   "tcp4 192.168.1.142:52544<->140.82.112.26:443"
//   "tcp6 fe80::1%en0.51806<->fe80::2%en0.61898"
//   "quic4 192.168.1.142:51163<->151.101.201.64:443"
// Returns None for wildcards ("*") and anything unparseable.
fn parse_remote_ip(label: &str) -> Option<IpAddr> {
    let (_, remote) = label.split_once("<->")?;
    if remote.starts_with('*') {
        return None;
    }
    // IPv4 uses ':' between IP and port; IPv6 uses '.'. Guess based on
    // presence of '::' which only appears in IPv6.
    let is_v6 = label.contains("6 ") || remote.contains("::");
    let separator = if is_v6 { '.' } else { ':' };
    let (ip_str, _port) = remote.rsplit_once(separator)?;
    // Strip the scope zone identifier from link-local IPv6 (fe80::…%en0).
    let ip_str = match ip_str.split_once('%') {
        Some((ip, _zone)) => ip,
        None => ip_str,
    };
    ip_str.parse().ok()
}

// Filter out addresses the user wouldn't call "servers I'm hitting":
// loopback, link-local, unspecified, multicast, and broadcast.
fn is_ignorable(ip: &IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => v4.is_broadcast() || v4.is_link_local(),
        // Ipv6Addr::is_unicast_link_local is unstable; check the fe80::/10 prefix by hand.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

// Smoothed per-remote-host traffic within one app.
pub struct HostTraffic {
    pub ema_bps_in: f64,
    pub ema_bps_out: f64,
    pub conn_count: u32,
    pub last_active: Instant,
}

impl HostTraffic {
    pub fn total_ema_bps(&self) -> f64 {
        self.ema_bps_in + self.ema_bps_out
    }
}

// Smoothed, sort-stable snapshot of one application built up over many
// samples. Kept separate from `NetSample` (which is per-interval delta)
// so the UI can render smoothly even if samples are late or missing.
pub struct AppStat {
    pub name: String,
    // Exponentially-smoothed bytes/sec, separate directions so the UI
    // can annotate ↓/↑ without losing signal.
    pub ema_bps_in: f64,
    pub ema_bps_out: f64,
    // Rolling history of *total* (in+out) bytes/sec. Newest at back.
    // Feeds the sparkline and the peak-based sort key.
    pub history: VecDeque<u32>,
    // Longer per-direction histories (bytes/sec, newest at back) feeding
    // the charts in the app detail view.
    pub history_in: VecDeque<u32>,
    pub history_out: VecDeque<u32>,
    pub conn_count: u32,
    // Per-remote-host breakdown, smoothed the same way as the app totals.
    pub hosts: HashMap<IpAddr, HostTraffic>,
    // Last sample where the app had non-zero traffic — drives fade-out.
    pub last_active: Instant,
}

impl AppStat {
    pub fn total_ema_bps(&self) -> f64 {
        self.ema_bps_in + self.ema_bps_out
    }

    // Peak of the rolling window — used to sort. Naturally decays as old
    // samples fall off the front, so a burst-then-quiet app stays high
    // for exactly HISTORY_LEN samples then drops.
    pub fn peak_bps(&self) -> u32 {
        self.history.iter().copied().max().unwrap_or(0)
    }

    // Busiest remote hosts for this app, by smoothed total rate.
    pub fn top_hosts(&self, n: usize) -> Vec<(IpAddr, &HostTraffic)> {
        let mut v: Vec<(IpAddr, &HostTraffic)> =
            self.hosts.iter().map(|(ip, h)| (*ip, h)).collect();
        v.sort_by(|a, b| {
            b.1.total_ema_bps()
                .partial_cmp(&a.1.total_ema_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
        });
        v.truncate(n);
        v
    }
}

pub struct NetworkState {
    pub apps: HashMap<String, AppStat>,
    // Whole-system smoothed throughput, for the header charts.
    pub ema_total_in: f64,
    pub ema_total_out: f64,
    // Whole-system instantaneous rate history (bytes/sec), newest at
    // back. Feeds the download/upload charts in the header.
    pub history_in: VecDeque<u32>,
    pub history_out: VecDeque<u32>,
    // Reverse-DNS cache. Background worker writes; UI reads.
    hostnames: Arc<Mutex<HashMap<IpAddr, String>>>,
    // IPs we've already queued for lookup, so `apply_sample` doesn't
    // re-enqueue every tick.
    dns_seen: HashSet<IpAddr>,
    dns_tx: Sender<IpAddr>,
    pub last_sample_at: Option<Instant>,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkState {
    pub fn new() -> Self {
        let (dns_tx, dns_rx) = mpsc::channel::<IpAddr>();
        let hostnames = Arc::new(Mutex::new(HashMap::new()));
        let cache = Arc::clone(&hostnames);
        thread::spawn(move || dns_worker(dns_rx, cache));
        Self {
            apps: HashMap::new(),
            ema_total_in: 0.0,
            ema_total_out: 0.0,
            history_in: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            history_out: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            hostnames,
            dns_seen: HashSet::new(),
            dns_tx,
            last_sample_at: None,
        }
    }

    // Fold one delta sample into the smoothed state.
    pub fn apply_sample(&mut self, sample: NetSample) {
        let now = Instant::now();
        let dt = sample.interval.as_secs_f64().max(0.001);
        // alpha for an EMA with time constant TAU sampled every dt secs.
        // Continuous-time correct: 1 - e^(-dt/TAU). Approaches 1 for
        // short TAU (very reactive) and 0 for long TAU (very sluggish).
        let alpha = 1.0 - (-dt / EMA_TAU_SECS).exp();

        // Snapshot which apps the sample carried so we can distinguish
        // "no traffic this tick" from "wasn't there at all".
        let seen: HashSet<String> = sample.apps.keys().cloned().collect();

        for (name, delta) in &sample.apps {
            let stat = self.apps.entry(name.clone()).or_insert_with(|| AppStat {
                name: name.clone(),
                ema_bps_in: 0.0,
                ema_bps_out: 0.0,
                history: VecDeque::with_capacity(HISTORY_LEN),
                history_in: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
                history_out: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
                conn_count: 0,
                hosts: HashMap::new(),
                last_active: now,
            });
            let bps_in = delta.bytes_in as f64 / dt;
            let bps_out = delta.bytes_out as f64 / dt;
            stat.ema_bps_in = stat.ema_bps_in * (1.0 - alpha) + bps_in * alpha;
            stat.ema_bps_out = stat.ema_bps_out * (1.0 - alpha) + bps_out * alpha;
            // History uses INSTANT rate (not EMA) — sparkline should
            // show the actual burstiness, not a pre-smoothed line.
            let total_now = (bps_in + bps_out).clamp(0.0, u32::MAX as f64) as u32;
            push_history(&mut stat.history, total_now, HISTORY_LEN);
            push_history(
                &mut stat.history_in,
                bps_in.clamp(0.0, u32::MAX as f64) as u32,
                TOTAL_HISTORY_LEN,
            );
            push_history(
                &mut stat.history_out,
                bps_out.clamp(0.0, u32::MAX as f64) as u32,
                TOTAL_HISTORY_LEN,
            );
            stat.conn_count = delta.conn_count;
            if delta.bytes_in + delta.bytes_out > 0 {
                stat.last_active = now;
            }

            // Per-host smoothing within the app, same EMA math.
            for (ip, hd) in &delta.hosts {
                let host = stat.hosts.entry(*ip).or_insert_with(|| HostTraffic {
                    ema_bps_in: 0.0,
                    ema_bps_out: 0.0,
                    conn_count: 0,
                    last_active: now,
                });
                let h_in = hd.bytes_in as f64 / dt;
                let h_out = hd.bytes_out as f64 / dt;
                host.ema_bps_in = host.ema_bps_in * (1.0 - alpha) + h_in * alpha;
                host.ema_bps_out = host.ema_bps_out * (1.0 - alpha) + h_out * alpha;
                host.conn_count = hd.conn_count;
                if hd.bytes_in + hd.bytes_out > 0 {
                    host.last_active = now;
                }
                if self.dns_seen.insert(*ip) {
                    let _ = self.dns_tx.send(*ip);
                }
            }
            // Decay hosts the sample didn't mention; drop long-idle ones.
            stat.hosts.retain(|ip, h| {
                if !delta.hosts.contains_key(ip) {
                    h.ema_bps_in *= 1.0 - alpha;
                    h.ema_bps_out *= 1.0 - alpha;
                    h.conn_count = 0;
                }
                now.duration_since(h.last_active).as_secs() < REMOVE_AFTER_SECS
            });
        }

        // Decay apps we still track but didn't see, so their sparkline
        // stays time-aligned and their rates fade toward zero.
        for (name, stat) in self.apps.iter_mut() {
            if !seen.contains(name) {
                stat.ema_bps_in *= 1.0 - alpha;
                stat.ema_bps_out *= 1.0 - alpha;
                stat.conn_count = 0;
                push_history(&mut stat.history, 0, HISTORY_LEN);
                push_history(&mut stat.history_in, 0, TOTAL_HISTORY_LEN);
                push_history(&mut stat.history_out, 0, TOTAL_HISTORY_LEN);
                for h in stat.hosts.values_mut() {
                    h.ema_bps_in *= 1.0 - alpha;
                    h.ema_bps_out *= 1.0 - alpha;
                    h.conn_count = 0;
                }
                stat.hosts.retain(|_, h| {
                    now.duration_since(h.last_active).as_secs() < REMOVE_AFTER_SECS
                });
            }
        }

        // System-wide totals — same smoothing, plus raw history for the
        // header charts.
        let sys_in = sample.total_in as f64 / dt;
        let sys_out = sample.total_out as f64 / dt;
        self.ema_total_in = self.ema_total_in * (1.0 - alpha) + sys_in * alpha;
        self.ema_total_out = self.ema_total_out * (1.0 - alpha) + sys_out * alpha;
        push_history(
            &mut self.history_in,
            sys_in.clamp(0.0, u32::MAX as f64) as u32,
            TOTAL_HISTORY_LEN,
        );
        push_history(
            &mut self.history_out,
            sys_out.clamp(0.0, u32::MAX as f64) as u32,
            TOTAL_HISTORY_LEN,
        );

        // Drop apps that have been silent long enough to warrant it.
        self.apps.retain(|_, s| {
            now.duration_since(s.last_active).as_secs() < REMOVE_AFTER_SECS
        });

        self.last_sample_at = Some(now);
    }

    // O(1) hostname lookup by IP — falls back to None if the reverse-DNS
    // worker hasn't finished (or gave up) yet.
    pub fn hostname_for(&self, ip: &IpAddr) -> Option<String> {
        self.hostnames.lock().ok()?.get(ip).cloned()
    }

    // Ordering the UI will render. Peak-based so a briefly-quiet app
    // doesn't lose its rank the moment its rate dips.
    pub fn sorted_apps(&self) -> Vec<&AppStat> {
        let mut v: Vec<&AppStat> = self.apps.values().collect();
        v.sort_by(|a, b| {
            b.peak_bps()
                .cmp(&a.peak_bps())
                // Tie-break on smoothed rate so two truly-idle apps have
                // a deterministic order rather than flickering by HashMap
                // iteration whim.
                .then_with(|| {
                    b.total_ema_bps()
                        .partial_cmp(&a.total_ema_bps())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.name.cmp(&b.name))
        });
        v
    }
}

fn push_history(h: &mut VecDeque<u32>, v: u32, cap: usize) {
    if h.len() >= cap {
        h.pop_front();
    }
    h.push_back(v);
}

// Serially resolves IPs via `dig -x`. Serial (not a pool) because we only
// look up new IPs — one every few seconds at steady state. Latency isn't
// user-visible; the UI just shows the IP until the name lands.
fn dns_worker(rx: Receiver<IpAddr>, cache: Arc<Mutex<HashMap<IpAddr, String>>>) {
    while let Ok(ip) = rx.recv() {
        let out = Command::new("dig")
            .args(["-x", &ip.to_string(), "+short", "+time=2", "+tries=1"])
            .stderr(Stdio::null())
            .output();
        let Ok(o) = out else { continue };
        if !o.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&o.stdout);
        let first = text
            .lines()
            .next()
            .map(|s| s.trim().trim_end_matches('.'))
            .unwrap_or("");
        if first.is_empty() {
            continue; // No PTR record — leave the IP as its own label.
        }
        if let Ok(mut c) = cache.lock() {
            c.insert(ip, first.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_pid_from_normal_names() {
        assert_eq!(strip_pid("firefox.935"), "firefox");
        assert_eq!(strip_pid("Code Helper.3745"), "Code Helper");
        assert_eq!(strip_pid("kernel_task.0"), "kernel_task");
    }

    #[test]
    fn strip_pid_leaves_versioned_names_alone() {
        // A name like "2.1.197.4801" (weird but real: seen on this Mac)
        // has a numeric tail and would otherwise be over-stripped. The
        // parser only cares that we get *something* consistent — the tail
        // stripping just needs to not panic.
        let stripped = strip_pid("2.1.197.4801");
        assert_eq!(stripped, "2.1.197");
    }

    #[test]
    fn canonicalizes_browser_helpers() {
        assert_eq!(canonical_app("Google Chrome Helper (Renderer)"), "Google Chrome");
        assert_eq!(canonical_app("Google Chrome Helper"), "Google Chrome");
        assert_eq!(canonical_app("firefox"), "Firefox");
        assert_eq!(canonical_app("plugin-container"), "Firefox");
        assert_eq!(canonical_app("com.apple.WebKit.Networking"), "Safari");
        assert_eq!(canonical_app("Safari"), "Safari");
        assert_eq!(canonical_app("Slack Helper (GPU)"), "Slack");
        assert_eq!(canonical_app("Code Helper (Plugin)"), "Code");
        assert_eq!(canonical_app("nettop"), "nettop");
    }

    #[test]
    fn canonicalizes_claude_code_version_binary() {
        // Claude Code's native install runs an executable named after the
        // bare version, e.g. "~/.local/share/claude/versions/2.1.198".
        assert_eq!(canonical_app("2.1.198"), "Claude Code");
        assert_eq!(canonical_app("10.0"), "Claude Code");
        // Names that merely contain digits/dots but aren't pure version
        // strings pass through untouched.
        assert_eq!(canonical_app("iTerm2"), "iTerm2");
        assert_eq!(canonical_app("2.1.198beta"), "2.1.198beta");
        assert_eq!(canonical_app("509"), "509");
        assert_eq!(canonical_app(".198"), ".198");
    }

    #[test]
    fn parses_tcp4_remote() {
        let ip = parse_remote_ip("tcp4 192.168.1.142:52544<->140.82.112.26:443").unwrap();
        assert_eq!(ip.to_string(), "140.82.112.26");
    }

    #[test]
    fn parses_tcp6_remote_with_zone_id() {
        let ip = parse_remote_ip(
            "tcp6 fe80::db:feb2:5294:184a%en0.51806<->fe80::14b9:a771:6b66:b271%en0.61898",
        )
        .unwrap();
        assert_eq!(ip.to_string(), "fe80::14b9:a771:6b66:b271");
    }

    #[test]
    fn parses_quic4_remote() {
        let ip = parse_remote_ip("quic4 192.168.1.142:51163<->151.101.201.64:443").unwrap();
        assert_eq!(ip.to_string(), "151.101.201.64");
    }

    #[test]
    fn rejects_wildcard_and_listen_rows() {
        assert!(parse_remote_ip("udp4 *:*<->*:*").is_none());
        assert!(parse_remote_ip("tcp4 *:51806<->*:*").is_none());
    }

    #[test]
    fn ignores_loopback_and_link_local() {
        assert!(is_ignorable(&"127.0.0.1".parse().unwrap()));
        assert!(is_ignorable(&"::1".parse().unwrap()));
        assert!(is_ignorable(&"169.254.1.1".parse().unwrap()));
        assert!(is_ignorable(&"fe80::1".parse().unwrap()));
        // Private but not link-local — these *are* servers the user hits
        // (home NAS, printers, etc), so we must keep them.
        assert!(!is_ignorable(&"192.168.1.22".parse().unwrap()));
        assert!(!is_ignorable(&"10.0.0.1".parse().unwrap()));
    }

    // Mirrors real `nettop -x -n -L 1` output: header row, then process
    // rows each followed by their connection rows, cumulative counters.
    const SNAP_1: &str = "\
,bytes_in,bytes_out,
firefox.935,999999999,888888888,
tcp4 192.168.1.142:52541<->151.101.1.91:443,999999000,888888000,
tcp4 192.168.1.142:52539<->35.186.224.31:443,999,888,
";
    const SNAP_2: &str = "\
,bytes_in,bytes_out,
firefox.935,1000000699,888888388,
tcp4 192.168.1.142:52541<->151.101.1.91:443,999999700,888888300,
tcp4 192.168.1.142:52539<->35.186.224.31:443,1299,1088,
";

    #[test]
    fn first_snapshot_is_all_baseline() {
        // Everything in the first snapshot predates the monitor — its
        // huge cumulative counters must register as zero traffic, but
        // the connection inventory should still come through.
        let mut t = DeltaTracker::new();
        let sample = t.build_sample(SNAP_1);
        assert_eq!(sample.total_in, 0);
        assert_eq!(sample.total_out, 0);
        assert_eq!(sample.apps["Firefox"].conn_count, 2);
    }

    #[test]
    fn diffs_cumulative_counters_between_snapshots() {
        let mut t = DeltaTracker::new();
        t.build_sample(SNAP_1);
        let sample = t.build_sample(SNAP_2);
        assert_eq!(sample.total_in, 1000); // 700 + 300
        assert_eq!(sample.total_out, 500); // 300 + 200
        let app = &sample.apps["Firefox"];
        assert_eq!(app.bytes_in, 1000);
        assert_eq!(app.bytes_out, 500);
        assert_eq!(app.conn_count, 2);
        assert_eq!(app.hosts.len(), 2);
        let host_a: IpAddr = "151.101.1.91".parse().unwrap();
        assert_eq!(app.hosts[&host_a].bytes_in, 700);
        assert_eq!(app.hosts[&host_a].bytes_out, 300);
    }

    #[test]
    fn connection_appearing_after_priming_counts_in_full() {
        // A connection born between snapshots: its whole counter is this
        // interval's traffic.
        let mut t = DeltaTracker::new();
        t.build_sample(",bytes_in,bytes_out,\n");
        let sample = t.build_sample(SNAP_1);
        assert_eq!(sample.total_in, 999999999);
        assert_eq!(sample.total_out, 888888888);
    }

    #[test]
    fn counter_regression_reads_as_fresh_connection() {
        // Same 4-tuple, smaller counter ⇒ the old connection closed and
        // a new one took its place; count the new one's bytes.
        let mut t = DeltaTracker::new();
        t.build_sample(SNAP_1);
        let sample = t.build_sample(
            "\
,bytes_in,bytes_out,
firefox.935,150,60,
tcp4 192.168.1.142:52541<->151.101.1.91:443,150,60,
",
        );
        assert_eq!(sample.total_in, 150);
        assert_eq!(sample.total_out, 60);
    }

    #[test]
    fn bundles_helper_processes_into_one_app() {
        // Two Chrome helper processes talking to the same remote host
        // must fold into a single "Google Chrome" aggregate — but their
        // identical connection descriptors must NOT collide in the delta
        // tracker (the pid-bearing label keeps the keys distinct).
        let mut t = DeltaTracker::new();
        t.build_sample(
            "\
,bytes_in,bytes_out,
Google Chrome Helper (GPU).412,0,0,
tcp4 192.168.1.142:52541<->142.250.72.14:443,0,0,
Google Chrome Helper (Renderer).977,0,0,
tcp4 192.168.1.142:52541<->142.250.72.14:443,0,0,
",
        );
        let sample = t.build_sample(
            "\
,bytes_in,bytes_out,
Google Chrome Helper (GPU).412,100,10,
tcp4 192.168.1.142:52541<->142.250.72.14:443,100,10,
Google Chrome Helper (Renderer).977,200,20,
tcp4 192.168.1.142:52541<->142.250.72.14:443,200,20,
",
        );
        assert_eq!(sample.apps.len(), 1);
        let app = &sample.apps["Google Chrome"];
        assert_eq!(app.bytes_in, 300);
        assert_eq!(app.bytes_out, 30);
        assert_eq!(app.conn_count, 2);
        let host: IpAddr = "142.250.72.14".parse().unwrap();
        assert_eq!(app.hosts[&host].bytes_in, 300);
        assert_eq!(app.hosts[&host].conn_count, 2);
    }
}
