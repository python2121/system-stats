//! Per-application bandwidth monitor built on top of macOS' `nettop`.
//!
//! `nettop -d` gives us per-interval deltas (rather than cumulative
//! counters), which sidesteps a class of tracking bugs — we just parse and
//! forward. `-n` keeps hostname resolution off so a mid-stream `IP → name`
//! rewrite doesn't split a single connection across two aggregation keys.
//!
//! Traffic is grouped by *application* first (all `Google Chrome Helper`
//! variants collapse into "Google Chrome", `plugin-container` joins
//! Firefox, and so on) with a per-remote-host breakdown kept inside each
//! app so the UI can show "who is this app talking to".

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
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

// Sample delimiter: nettop -x -L reprints the column-name row between
// each sample. We use that to detect boundaries.
const HEADER_ROW: &str = ",bytes_in,bytes_out,";

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
    // Owned so nettop is killed via Drop when the Monitor is dropped.
    // Never read after construction — the field is here for its lifetime.
    _child: ChildGuard,
}

impl Monitor {
    pub fn try_recv(&self) -> Result<NetSample, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

// RAII wrapper: kill nettop when the Monitor goes out of scope so a
// panicking UI doesn't leave orphaned children behind. We spawn `script`
// as a middleman (see `spawn_monitor` for why) so cleanup has to nuke
// the whole process group — SIGKILL'ing `script` alone leaves `nettop`
// orphaned since macOS doesn't reliably SIGHUP the pty slave.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let pid = self.0.id() as i32;
        // `kill -TERM -pgid` (negative pid) targets the whole process
        // group. Fall back to SIGKILL if TERM didn't take within 200ms.
        // Shell out rather than pull in libc for a one-liner.
        let _ = Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
        thread::sleep(Duration::from_millis(200));
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
        let _ = self.0.wait();
    }
}

// Returns None if nettop couldn't be spawned — caller falls back to
// showing an error state instead of crashing.
pub fn spawn_monitor() -> Option<Monitor> {
    // Nettop line-buffers on a tty but block-buffers on a pipe, so a
    // straight `Stdio::piped()` reader would only see ~10s bursts. We
    // wrap it in `script -q /dev/null` which gives it a pty — output
    // then streams at the -s interval as advertised.
    //
    // process_group(0) puts the whole child chain in a fresh pgid so
    // ChildGuard::drop can nuke script AND nettop with one kill(2) at
    // shutdown.
    let mut child = Command::new("script")
        .args([
            "-q", "/dev/null",
            "nettop",
            "-x", "-n", "-d",
            "-L", "0",
            "-s", &SAMPLE_INTERVAL.as_secs().to_string(),
            "-J", "bytes_in,bytes_out",
            "-t", "external",
        ])
        // Detach from the parent's stdin. If we inherit the parent's fd
        // and the parent is a TUI (raw-mode tty), `script` gets confused
        // trying to save/restore terminal attributes on a fd that's
        // shared with a live curses app. /dev/null keeps it out of the
        // way — script doesn't need to read anything anyway.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut builder = SampleBuilder::new();
        for line in reader.lines().map_while(Result::ok) {
            // `contains` rather than `starts_with` because `script(1)`
            // prepends a stray `^D` (and sometimes a couple of backspace
            // chars) at stream start, and occasional control bytes can
            // slip in from the pty.
            if line.contains(HEADER_ROW) {
                if let Some(sample) = builder.finish() {
                    if tx.send(sample).is_err() {
                        // Receiver dropped — the UI is going away.
                        return;
                    }
                }
                continue;
            }
            builder.push_row(&line);
        }
    });

    Some(Monitor {
        rx,
        _child: ChildGuard(child),
    })
}

// Accumulates rows until a sample delimiter arrives. Data lives
// *between* headers, so on the Nth header we're finishing the (N-1)th
// sample:
//   header #1 → nothing collected yet (return None)
//   header #2 → sample #1 done — but that's the cumulative baseline, drop
//   header #3+ → real delta sample, emit
struct SampleBuilder {
    apps: HashMap<String, AppDelta>,
    total_in: u64,
    total_out: u64,
    // The canonical app a subsequent connection row belongs to. Nettop
    // emits process rows followed by their connection children, so we
    // remember whichever process we last saw.
    current_app: Option<String>,
    // Count of header rows seen so far. Cheap way to know "am I past the
    // baseline sample" without extra boolean state.
    headers_seen: u32,
}

impl SampleBuilder {
    fn new() -> Self {
        Self {
            apps: HashMap::new(),
            total_in: 0,
            total_out: 0,
            current_app: None,
            headers_seen: 0,
        }
    }

    fn push_row(&mut self, line: &str) {
        let Some(row) = parse_row(line) else { return };
        match row {
            Row::Process { name } => {
                self.current_app = Some(canonical_app(name));
            }
            Row::Connection { remote_ip, bytes_in, bytes_out } => {
                if is_ignorable(&remote_ip) {
                    return;
                }
                let app_name = self
                    .current_app
                    .clone()
                    .unwrap_or_else(|| "(unknown)".to_string());
                let app = self.apps.entry(app_name).or_default();
                let host = app.hosts.entry(remote_ip).or_default();
                // Idle connections still count toward conn totals so the
                // UI can show "34 conns" even when most are quiet.
                app.conn_count += 1;
                host.conn_count += 1;
                if bytes_in == 0 && bytes_out == 0 {
                    return;
                }
                app.bytes_in += bytes_in;
                app.bytes_out += bytes_out;
                host.bytes_in += bytes_in;
                host.bytes_out += bytes_out;
                self.total_in += bytes_in;
                self.total_out += bytes_out;
            }
        }
    }

    fn finish(&mut self) -> Option<NetSample> {
        // First header (#1): nothing collected yet, don't emit.
        // Second header (#2): what was collected between #1 and #2 is
        //   the cumulative baseline — drop it.
        // Third+ (#3+): the data between successive headers is a real
        //   delta sample; emit.
        self.headers_seen += 1;
        let out = if self.headers_seen >= 3 {
            Some(NetSample {
                interval: SAMPLE_INTERVAL,
                apps: std::mem::take(&mut self.apps),
                total_in: self.total_in,
                total_out: self.total_out,
            })
        } else {
            None
        };
        // Reset accumulators for the next sample.
        self.apps = HashMap::new();
        self.total_in = 0;
        self.total_out = 0;
        self.current_app = None;
        out
    }
}

enum Row<'a> {
    Process { name: &'a str },
    Connection { remote_ip: IpAddr, bytes_in: u64, bytes_out: u64 },
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
        Some(Row::Connection { remote_ip, bytes_in, bytes_out })
    } else {
        // Process row: "<name>.<pid>". Strip the ".pid" suffix so multiple
        // pids of the same binary aggregate.
        let name = strip_pid(label);
        Some(Row::Process { name })
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

    #[test]
    fn parses_a_full_sample_from_captured_nettop() {
        // Two consecutive rows from the captured `nettop -x -n -d` output.
        // Both belong to firefox.935 — they should aggregate into one app
        // ("Firefox") with two distinct remote hosts. We prime past the
        // baseline first (two header finishes) so the third sample is a
        // real emitted one.
        let mut b = SampleBuilder::new();
        assert!(b.finish().is_none()); // #1: pre-first-header
        assert!(b.finish().is_none()); // #2: cumulative baseline drop
        b.push_row("firefox.935,1000,500,");
        b.push_row("tcp4 192.168.1.142:52541<->151.101.1.91:443,700,300,");
        b.push_row("tcp4 192.168.1.142:52539<->35.186.224.31:443,300,200,");
        let sample = b.finish().unwrap();
        assert_eq!(sample.total_in, 1000);
        assert_eq!(sample.total_out, 500);
        assert_eq!(sample.apps.len(), 1);
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
    fn bundles_helper_processes_into_one_app() {
        // Two Chrome helper processes talking to the same remote host
        // must fold into a single "Google Chrome" aggregate.
        let mut b = SampleBuilder::new();
        assert!(b.finish().is_none());
        assert!(b.finish().is_none());
        b.push_row("Google Chrome Helper (GPU).412,100,10,");
        b.push_row("tcp4 192.168.1.142:52541<->142.250.72.14:443,100,10,");
        b.push_row("Google Chrome Helper (Renderer).977,200,20,");
        b.push_row("tcp4 192.168.1.142:52542<->142.250.72.14:443,200,20,");
        let sample = b.finish().unwrap();
        assert_eq!(sample.apps.len(), 1);
        let app = &sample.apps["Google Chrome"];
        assert_eq!(app.bytes_in, 300);
        assert_eq!(app.bytes_out, 30);
        assert_eq!(app.conn_count, 2);
        assert_eq!(app.hosts.len(), 1);
        let host: IpAddr = "142.250.72.14".parse().unwrap();
        assert_eq!(app.hosts[&host].bytes_in, 300);
        assert_eq!(app.hosts[&host].conn_count, 2);
    }

    #[test]
    fn drops_the_baseline_sample() {
        // Simulate the nettop stream: header, baseline data, header,
        // first real delta data, header. The baseline (huge cumulative
        // values) must be dropped; only the real delta emerges.
        let mut b = SampleBuilder::new();
        // First header seen with no data yet.
        assert!(b.finish().is_none(), "pre-baseline header returns None");
        // Baseline row values.
        b.push_row("firefox.935,999999999,999999999,");
        b.push_row("tcp4 192.168.1.142:52541<->151.101.1.91:443,999999999,999999999,");
        assert!(b.finish().is_none(), "baseline sample must not be emitted");
        // Real delta.
        b.push_row("firefox.935,100,50,");
        b.push_row("tcp4 192.168.1.142:52541<->151.101.1.91:443,100,50,");
        let sample = b.finish().expect("first real sample is emitted");
        assert_eq!(sample.total_in, 100);
        assert_eq!(sample.total_out, 50);
    }
}
