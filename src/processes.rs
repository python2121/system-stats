//! Per-application CPU/memory monitor built on the `sysinfo` crate.
//!
//! Mirrors the network module's shape: a background thread samples on a
//! fixed cadence and sends immutable `ProcSample`s over a channel; the UI
//! thread folds them into `ProcessState`, which keeps EMA-smoothed rates
//! and rolling histories so rendering stays steady between samples.
//!
//! Processes are grouped by *application* using the same canonicalization
//! as the network tab ("Google Chrome Helper (Renderer)" → "Google
//! Chrome"), with the member processes kept inside each app so the UI can
//! show "what is this app made of".

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{ProcessesToUpdate, System};

use crate::network::canonical_app;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
// Same smoothing time constant as the network tab so the two feel alike.
pub const EMA_TAU_SECS: f64 = 5.0;
// Sparkline + sort window: 30 samples at 2s = 60s.
pub const HISTORY_LEN: usize = 30;
// Header / detail charts: 240 samples at 2s = 8 minutes.
pub const TOTAL_HISTORY_LEN: usize = 240;
// List cutoff: apps that never reached this CPU (deci-percent) in the
// sort window and hold less than MIN_VISIBLE_MEM stay hidden so hundreds
// of idle daemons don't bury the interesting rows.
pub const MIN_VISIBLE_CPU_DECI: u32 = 1; // 0.1%
pub const MIN_VISIBLE_MEM: u64 = 50 * 1024 * 1024;

pub struct ProcSample {
    pub interval: Duration,
    // Whole-system CPU usage, 0–100 (average across cores).
    pub cpu_total: f32,
    pub core_count: usize,
    // 1-minute load average.
    pub load_avg: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    // Per-application totals, keyed by canonical app name.
    pub apps: HashMap<String, AppDelta>,
}

#[derive(Default)]
pub struct AppDelta {
    // Sum of member processes' CPU, in percent-of-one-core (can exceed
    // 100 for multi-process apps — same convention as Activity Monitor).
    pub cpu: f32,
    // Sum of member processes' resident memory, bytes.
    pub mem: u64,
    pub procs: Vec<ProcInfo>,
}

#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
}

pub struct Monitor {
    rx: Receiver<ProcSample>,
}

impl Monitor {
    pub fn try_recv(&self) -> Result<ProcSample, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

// Unlike the network monitor there's no child process to babysit — the
// sampler is a plain thread that exits when the UI drops the receiver.
pub fn spawn_monitor() -> Monitor {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut sys = System::new();
        let mut first = true;
        loop {
            sys.refresh_memory();
            sys.refresh_cpu_usage();
            sys.refresh_processes(ProcessesToUpdate::All, true);
            if first {
                // The first refresh has no earlier reading to diff
                // against, so every CPU number reads 0. Drop it — the
                // same "baseline sample" treatment nettop gets.
                first = false;
            } else {
                let mut apps: HashMap<String, AppDelta> = HashMap::new();
                for p in sys.processes().values() {
                    let name = p.name().to_string_lossy().to_string();
                    let cpu = p.cpu_usage();
                    let mem = p.memory();
                    let app = apps.entry(canonical_app(&name)).or_default();
                    app.cpu += cpu;
                    app.mem += mem;
                    app.procs.push(ProcInfo { pid: p.pid().as_u32(), name, cpu, mem });
                }
                let sample = ProcSample {
                    interval: SAMPLE_INTERVAL,
                    cpu_total: sys.global_cpu_usage(),
                    core_count: sys.cpus().len(),
                    load_avg: System::load_average().one,
                    mem_used: sys.used_memory(),
                    mem_total: sys.total_memory(),
                    apps,
                };
                if tx.send(sample).is_err() {
                    // Receiver dropped — the UI is going away.
                    return;
                }
            }
            thread::sleep(SAMPLE_INTERVAL);
        }
    });
    Monitor { rx }
}

// Smoothed, sort-stable snapshot of one application built up over many
// samples — the processes-tab analog of the network tab's AppStat.
pub struct AppStat {
    pub name: String,
    // Smoothed percent-of-one-core.
    pub ema_cpu: f64,
    // Resident memory, bytes — latest sample, no smoothing (memory
    // doesn't flicker the way CPU does).
    pub mem: u64,
    // Rolling CPU history in deci-percent (0.1% resolution keeps quiet
    // apps visible without floats). Newest at back. Feeds the sparkline
    // and the peak-based sort key.
    pub history: VecDeque<u32>,
    // Longer histories feeding the detail-view charts: CPU in
    // deci-percent, memory in MiB.
    pub history_cpu: VecDeque<u32>,
    pub history_mem: VecDeque<u32>,
    // Member processes from the latest sample, busiest first.
    pub procs: Vec<ProcInfo>,
    // Last sample where the app used noticeable CPU — drives fade-out.
    pub last_active: Instant,
}

impl AppStat {
    // Peak of the rolling window — used to sort. Naturally decays as old
    // samples fall off the front.
    pub fn peak_cpu_deci(&self) -> u32 {
        self.history.iter().copied().max().unwrap_or(0)
    }
}

pub struct ProcessState {
    pub apps: HashMap<String, AppStat>,
    // Whole-system smoothed CPU (0–100) for the header chart.
    pub ema_cpu_total: f64,
    pub core_count: usize,
    pub load_avg: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    // Whole-system instantaneous histories, newest at back: CPU in
    // deci-percent, memory used in MiB.
    pub history_cpu: VecDeque<u32>,
    pub history_mem: VecDeque<u32>,
    pub last_sample_at: Option<Instant>,
}

impl Default for ProcessState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessState {
    pub fn new() -> Self {
        Self {
            apps: HashMap::new(),
            ema_cpu_total: 0.0,
            core_count: 0,
            load_avg: 0.0,
            mem_used: 0,
            mem_total: 0,
            history_cpu: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            history_mem: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            last_sample_at: None,
        }
    }

    // Fold one sample into the smoothed state.
    pub fn apply_sample(&mut self, sample: ProcSample) {
        let now = Instant::now();
        let dt = sample.interval.as_secs_f64().max(0.001);
        // Same continuous-time EMA as the network tab.
        let alpha = 1.0 - (-dt / EMA_TAU_SECS).exp();

        // Unlike nettop (which only reports apps with traffic), sysinfo
        // reports every live process each tick — an app missing from the
        // sample has exited, so drop it entirely.
        self.apps.retain(|name, _| sample.apps.contains_key(name));

        for (name, delta) in sample.apps {
            let stat = self.apps.entry(name.clone()).or_insert_with(|| AppStat {
                name,
                ema_cpu: 0.0,
                mem: 0,
                history: VecDeque::with_capacity(HISTORY_LEN),
                history_cpu: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
                history_mem: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
                procs: Vec::new(),
                last_active: now,
            });
            stat.ema_cpu = stat.ema_cpu * (1.0 - alpha) + delta.cpu as f64 * alpha;
            stat.mem = delta.mem;
            // History uses the INSTANT reading (not EMA) — the sparkline
            // should show real burstiness, matching the network tab.
            let deci = (delta.cpu as f64 * 10.0).round().max(0.0) as u32;
            push_history(&mut stat.history, deci, HISTORY_LEN);
            push_history(&mut stat.history_cpu, deci, TOTAL_HISTORY_LEN);
            push_history(
                &mut stat.history_mem,
                (delta.mem / (1024 * 1024)) as u32,
                TOTAL_HISTORY_LEN,
            );
            let mut procs = delta.procs;
            procs.sort_by(|a, b| {
                b.cpu
                    .partial_cmp(&a.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.mem.cmp(&a.mem))
                    .then_with(|| a.pid.cmp(&b.pid))
            });
            stat.procs = procs;
            if delta.cpu >= 0.1 {
                stat.last_active = now;
            }
        }

        self.ema_cpu_total =
            self.ema_cpu_total * (1.0 - alpha) + sample.cpu_total as f64 * alpha;
        self.core_count = sample.core_count;
        self.load_avg = sample.load_avg;
        self.mem_used = sample.mem_used;
        self.mem_total = sample.mem_total;
        push_history(
            &mut self.history_cpu,
            (sample.cpu_total as f64 * 10.0).round().max(0.0) as u32,
            TOTAL_HISTORY_LEN,
        );
        push_history(
            &mut self.history_mem,
            (sample.mem_used / (1024 * 1024)) as u32,
            TOTAL_HISTORY_LEN,
        );

        self.last_sample_at = Some(now);
    }

    // Ordering the UI will render: window-peak CPU first (so a briefly
    // quiet app keeps its rank), then memory, then name for stability.
    pub fn sorted_apps(&self) -> Vec<&AppStat> {
        let mut v: Vec<&AppStat> = self.apps.values().collect();
        v.sort_by(|a, b| {
            b.peak_cpu_deci()
                .cmp(&a.peak_cpu_deci())
                .then_with(|| b.mem.cmp(&a.mem))
                .then_with(|| a.name.cmp(&b.name))
        });
        v
    }

    // sorted_apps minus the long tail of idle daemons. This is the list
    // the UI actually shows and the selection moves through.
    pub fn visible_apps(&self) -> Vec<&AppStat> {
        self.sorted_apps()
            .into_iter()
            .filter(|a| a.peak_cpu_deci() >= MIN_VISIBLE_CPU_DECI || a.mem >= MIN_VISIBLE_MEM)
            .collect()
    }

    pub fn total_proc_count(&self) -> usize {
        self.apps.values().map(|a| a.procs.len()).sum()
    }
}

fn push_history(h: &mut VecDeque<u32>, v: u32, cap: usize) {
    if h.len() >= cap {
        h.pop_front();
    }
    h.push_back(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(apps: Vec<(&str, f32, u64)>) -> ProcSample {
        ProcSample {
            interval: SAMPLE_INTERVAL,
            cpu_total: 10.0,
            core_count: 8,
            load_avg: 1.0,
            mem_used: 8 * 1024 * 1024 * 1024,
            mem_total: 16 * 1024 * 1024 * 1024,
            apps: apps
                .into_iter()
                .map(|(name, cpu, mem)| {
                    let delta = AppDelta {
                        cpu,
                        mem,
                        procs: vec![ProcInfo {
                            pid: 1,
                            name: name.to_string(),
                            cpu,
                            mem,
                        }],
                    };
                    (name.to_string(), delta)
                })
                .collect(),
        }
    }

    #[test]
    fn removes_apps_whose_processes_exited() {
        let mut state = ProcessState::new();
        state.apply_sample(sample(vec![("Firefox", 12.0, 1 << 30), ("make", 80.0, 1 << 20)]));
        assert_eq!(state.apps.len(), 2);
        // `make` finished — it's absent from the next sample.
        state.apply_sample(sample(vec![("Firefox", 9.0, 1 << 30)]));
        assert!(state.apps.contains_key("Firefox"));
        assert!(!state.apps.contains_key("make"));
    }

    #[test]
    fn hides_idle_daemons_but_keeps_memory_hogs() {
        let mut state = ProcessState::new();
        state.apply_sample(sample(vec![
            ("busy", 5.0, 1 << 20),
            ("hog", 0.0, 200 * 1024 * 1024),
            ("daemon", 0.0, 1 << 20),
        ]));
        let visible: Vec<&str> =
            state.visible_apps().iter().map(|a| a.name.as_str()).collect();
        assert!(visible.contains(&"busy"));
        assert!(visible.contains(&"hog"));
        assert!(!visible.contains(&"daemon"));
        // Hidden ≠ gone: the daemon is still tracked for when it wakes up.
        assert!(state.apps.contains_key("daemon"));
    }

    #[test]
    fn smooths_cpu_and_keeps_instant_history() {
        let mut state = ProcessState::new();
        state.apply_sample(sample(vec![("app", 100.0, 0)]));
        state.apply_sample(sample(vec![("app", 0.0, 0)]));
        let stat = &state.apps["app"];
        // EMA decays gradually …
        assert!(stat.ema_cpu > 0.0 && stat.ema_cpu < 100.0);
        // … but the history records the raw instantaneous readings.
        assert_eq!(stat.history.iter().copied().collect::<Vec<_>>(), vec![1000, 0]);
    }

    #[test]
    fn sorts_by_window_peak_then_memory() {
        let mut state = ProcessState::new();
        state.apply_sample(sample(vec![
            ("spiky", 50.0, 1 << 20),
            ("steady", 10.0, 1 << 20),
            ("hog", 0.0, 1 << 30),
        ]));
        // Spiky goes quiet but its window peak keeps it ranked first.
        state.apply_sample(sample(vec![
            ("spiky", 0.0, 1 << 20),
            ("steady", 10.0, 1 << 20),
            ("hog", 0.0, 1 << 30),
        ]));
        let order: Vec<&str> =
            state.sorted_apps().iter().map(|a| a.name.as_str()).collect();
        assert_eq!(order, vec!["spiky", "steady", "hog"]);
    }
}
