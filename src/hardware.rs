//! Disk + power telemetry for the "Disk / Power" tab.
//!
//! This file is platform-neutral: the sample/state types, the sampler
//! threads, and the EMA/history folding. Actually reading the numbers is
//! delegated to a per-OS backend (`hardware/macos.rs`, `hardware/linux.rs`)
//! selected at compile time; each exposes the same four functions
//! returning the same snapshot types, so everything above them compiles
//! unchanged on every platform. Fields a platform can't fill (e.g.
//! per-cell voltages outside macOS) stay None/empty and their UI panels
//! simply don't render.
//!
//! Same shape as the network/processes modules: a sampler thread sends
//! immutable samples over a channel; `HardwareState` folds them into
//! EMA-smoothed rates and rolling histories.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

// Any other OS: the tab renders with everything dark until someone
// writes a backend for it.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::{BatterySnapshot, DiskCounters, UnmountedVolume, VolumeInfo};
    pub fn battery() -> Option<BatterySnapshot> {
        None
    }
    pub fn disk_counters() -> Option<DiskCounters> {
        None
    }
    pub fn read_volumes() -> Vec<VolumeInfo> {
        Vec::new()
    }
    pub fn scan_unmounted() -> Option<Vec<UnmountedVolume>> {
        None
    }
}

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
pub const EMA_TAU_SECS: f64 = 5.0;
// 240 samples at 2s cadence = 8 minutes, matching the other tabs' charts.
pub const TOTAL_HISTORY_LEN: usize = 240;

pub struct HwSample {
    pub interval: Duration,
    // None when the machine has no battery (desktop) or the read failed.
    pub battery: Option<BatterySnapshot>,
    // Whole-package power/thermal telemetry for machines that expose it
    // (Linux hwmon). Fills the power panel on battery-less hardware.
    pub system: Option<SystemPowerSnapshot>,
    // Cumulative counters summed over every storage device. None if the
    // read failed.
    pub disk: Option<DiskCounters>,
    pub volumes: Vec<VolumeInfo>,
    // Filled only by the slow partition sweep (it can take seconds, so
    // it runs on its own thread at a 30s cadence). None = "no news this
    // tick".
    pub unmounted: Option<Vec<UnmountedVolume>>,
}

#[derive(Clone, Default)]
pub struct BatterySnapshot {
    // State-of-charge percent, 0–100.
    pub percent: i64,
    pub voltage_mv: i64,
    // Signed: negative while discharging, positive while charging.
    pub amperage_ma: i64,
    // Instantaneous battery power in watts, derived from the two above.
    // Negative = draining, positive = charging, ~0 = idle/on AC.
    pub watts: f64,
    pub temperature_c: f64,
    pub cycle_count: i64,
    pub design_capacity_mah: i64,
    // What a full charge actually holds today.
    pub max_capacity_mah: i64,
    pub current_capacity_mah: i64,
    pub is_charging: bool,
    pub external_connected: bool,
    pub fully_charged: bool,
    // Minutes; None when the estimate is invalid or unavailable.
    pub time_to_empty_min: Option<i64>,
    pub time_to_full_min: Option<i64>,
    // The remaining fields are macOS-only detail; other backends leave
    // them empty and the UI skips their panels.
    pub cell_voltages_mv: Vec<i64>,
    // Today's state-of-charge extremes, from the gauge's daily log.
    pub daily_min_soc: Option<i64>,
    pub daily_max_soc: Option<i64>,
    // Lifetime temperature extremes (°C) from the pack's flash log.
    pub lifetime_temp_min_c: Option<i64>,
    pub lifetime_temp_max_c: Option<i64>,
    pub adapter: Option<AdapterInfo>,
}

impl BatterySnapshot {
    pub fn health_percent(&self) -> f64 {
        if self.design_capacity_mah > 0 {
            self.max_capacity_mah as f64 / self.design_capacity_mah as f64 * 100.0
        } else {
            0.0
        }
    }
}

// Power draw and thermals for the machine as a whole rather than its
// battery — what a desktop (or a handheld's SoC) can report. On Linux
// this comes from hwmon: the amdgpu driver's PPT reading covers the
// whole APU package on AMD SoCs. Everything but the wattage is
// best-effort and optional.
#[derive(Clone, Default)]
pub struct SystemPowerSnapshot {
    // Package power draw in watts (amdgpu "PPT" — CPU+GPU on an APU).
    pub package_watts: f64,
    // Whether package_watts covers the CPU too: true on an APU (Steam
    // Deck), false when the sensor is a discrete GPU's (Steam Machine).
    // Drives the chart label — "package power" vs "gpu power".
    pub covers_cpu: bool,
    // The enforced power limit, when the driver exposes one.
    pub cap_watts: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub gpu_temp_c: Option<f64>,
    pub fan_rpm: Option<i64>,
    // GPU core rail voltage, millivolts.
    pub gpu_mv: Option<i64>,
}

#[derive(Clone, Default)]
pub struct AdapterInfo {
    pub watts: i64,
    pub voltage_mv: i64,
    pub current_ma: i64,
    pub is_wireless: bool,
    // Every USB-PD voltage profile the charger offered (volts, sorted).
    pub profile_volts: Vec<i64>,
}

#[derive(Clone, Copy, Default)]
pub struct DiskCounters {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    // Accumulated busy time, nanoseconds.
    pub read_time_ns: u64,
    pub write_time_ns: u64,
}

#[derive(Clone)]
pub struct VolumeInfo {
    pub name: String,
    pub mount: String,
    pub total: u64,
    pub available: u64,
    pub removable: bool,
}

impl VolumeInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn fill_frac(&self) -> f64 {
        if self.total > 0 {
            self.used() as f64 / self.total as f64
        } else {
            0.0
        }
    }
}

// A partition that exists on disk but isn't mounted — an unmountable
// foreign filesystem (a dual-boot OS), an ejected-but-attached volume,
// or an EFI system partition. The host can't report usage inside it; we
// show that it exists and how much disk it occupies.
#[derive(Clone)]
pub struct UnmountedVolume {
    pub name: String,
    // Device identifier, e.g. "disk0s6" (macOS) or "sda1" (Linux).
    pub device: String,
    // Filesystem personality or partition type, for the row's tag.
    pub kind: String,
    pub size: u64,
}

pub struct Monitor {
    rx: Receiver<HwSample>,
}

impl Monitor {
    pub fn try_recv(&self) -> Result<HwSample, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

// How often the slow sweep re-checks the partition table.
pub const UNMOUNTED_SCAN_INTERVAL: Duration = Duration::from_secs(30);

// Only the Linux backend has a system-power source so far; routing the
// call through this cfg'd shim (rather than a stub in every backend)
// keeps the macOS backend untouched.
fn sample_system_power() -> Option<SystemPowerSnapshot> {
    #[cfg(target_os = "linux")]
    {
        platform::system_power()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

pub fn spawn_monitor() -> Monitor {
    let (tx, rx) = mpsc::channel();
    let slow_tx = tx.clone();
    thread::spawn(move || {
        loop {
            let sample = HwSample {
                interval: SAMPLE_INTERVAL,
                battery: platform::battery(),
                system: sample_system_power(),
                disk: platform::disk_counters(),
                volumes: platform::read_volumes(),
                unmounted: None,
            };
            if tx.send(sample).is_err() {
                // Receiver dropped — the UI is going away.
                return;
            }
            thread::sleep(SAMPLE_INTERVAL);
        }
    });
    // The partition sweep can be slow (macOS' `diskutil info -all` takes
    // ~2s), so it gets its own thread and a relaxed cadence instead of
    // stalling the fast loop.
    thread::spawn(move || {
        loop {
            if let Some(unmounted) = platform::scan_unmounted() {
                let sample = HwSample {
                    interval: SAMPLE_INTERVAL,
                    battery: None,
                    system: None,
                    disk: None,
                    volumes: Vec::new(),
                    unmounted: Some(unmounted),
                };
                if slow_tx.send(sample).is_err() {
                    return;
                }
            }
            thread::sleep(UNMOUNTED_SCAN_INTERVAL);
        }
    });
    Monitor { rx }
}

// ---------- smoothed state ----------

pub struct HardwareState {
    pub battery: Option<BatterySnapshot>,
    // Smoothed battery flow, watts (negative = draining).
    pub ema_watts: f64,
    // Signed battery-flow history in deciwatts, newest at back.
    pub history_watts: VecDeque<i32>,
    pub system: Option<SystemPowerSnapshot>,
    // Smoothed package draw, watts.
    pub ema_package_w: f64,
    // Package-draw history in deciwatts, stored negative so the shared
    // flow chart renders it in the "draining" color.
    pub history_package: VecDeque<i32>,
    pub ema_read_bps: f64,
    pub ema_write_bps: f64,
    pub history_read: VecDeque<u32>,
    pub history_write: VecDeque<u32>,
    // Latest interval's op rates and mean latency (ms).
    pub read_iops: f64,
    pub write_iops: f64,
    pub read_lat_ms: f64,
    pub write_lat_ms: f64,
    pub volumes: Vec<VolumeInfo>,
    pub unmounted: Vec<UnmountedVolume>,
    prev_disk: Option<DiskCounters>,
    pub last_sample_at: Option<Instant>,
}

impl Default for HardwareState {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareState {
    pub fn new() -> Self {
        Self {
            battery: None,
            ema_watts: 0.0,
            history_watts: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            system: None,
            ema_package_w: 0.0,
            history_package: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            ema_read_bps: 0.0,
            ema_write_bps: 0.0,
            history_read: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            history_write: VecDeque::with_capacity(TOTAL_HISTORY_LEN),
            read_iops: 0.0,
            write_iops: 0.0,
            read_lat_ms: 0.0,
            write_lat_ms: 0.0,
            volumes: Vec::new(),
            unmounted: Vec::new(),
            prev_disk: None,
            last_sample_at: None,
        }
    }

    pub fn apply_sample(&mut self, sample: HwSample) {
        let now = Instant::now();
        let dt = sample.interval.as_secs_f64().max(0.001);
        let alpha = 1.0 - (-dt / EMA_TAU_SECS).exp();

        if let Some(bat) = &sample.battery {
            self.ema_watts = self.ema_watts * (1.0 - alpha) + bat.watts * alpha;
            let deci = (bat.watts * 10.0).round() as i32;
            push_history_signed(&mut self.history_watts, deci, TOTAL_HISTORY_LEN);
            self.battery = sample.battery;
        }

        if let Some(sys) = &sample.system {
            self.ema_package_w =
                self.ema_package_w * (1.0 - alpha) + sys.package_watts * alpha;
            let deci = -(sys.package_watts * 10.0).round() as i32;
            push_history_signed(&mut self.history_package, deci, TOTAL_HISTORY_LEN);
            self.system = sample.system.clone();
        }

        if let Some(cur) = sample.disk {
            if let Some(prev) = self.prev_disk {
                // Counters are cumulative-since-boot; a shrink means the
                // driver set changed (e.g. external ejected) — skip that
                // interval rather than chart a huge negative spike.
                let d_read = cur.read_bytes.checked_sub(prev.read_bytes);
                let d_write = cur.write_bytes.checked_sub(prev.write_bytes);
                if let (Some(dr), Some(dw)) = (d_read, d_write) {
                    let r_bps = dr as f64 / dt;
                    let w_bps = dw as f64 / dt;
                    self.ema_read_bps = self.ema_read_bps * (1.0 - alpha) + r_bps * alpha;
                    self.ema_write_bps =
                        self.ema_write_bps * (1.0 - alpha) + w_bps * alpha;
                    push_history(
                        &mut self.history_read,
                        r_bps.clamp(0.0, u32::MAX as f64) as u32,
                        TOTAL_HISTORY_LEN,
                    );
                    push_history(
                        &mut self.history_write,
                        w_bps.clamp(0.0, u32::MAX as f64) as u32,
                        TOTAL_HISTORY_LEN,
                    );

                    let d_rops = cur.read_ops.saturating_sub(prev.read_ops);
                    let d_wops = cur.write_ops.saturating_sub(prev.write_ops);
                    self.read_iops = d_rops as f64 / dt;
                    self.write_iops = d_wops as f64 / dt;
                    // Mean latency over the interval: busy-time delta per op.
                    self.read_lat_ms = if d_rops > 0 {
                        cur.read_time_ns.saturating_sub(prev.read_time_ns) as f64
                            / d_rops as f64
                            / 1e6
                    } else {
                        0.0
                    };
                    self.write_lat_ms = if d_wops > 0 {
                        cur.write_time_ns.saturating_sub(prev.write_time_ns) as f64
                            / d_wops as f64
                            / 1e6
                    } else {
                        0.0
                    };
                }
            }
            self.prev_disk = Some(cur);
        }

        if !sample.volumes.is_empty() {
            self.volumes = sample.volumes;
        }
        if let Some(unmounted) = sample.unmounted {
            self.unmounted = unmounted;
        }
        self.last_sample_at = Some(now);
    }
}

fn push_history(h: &mut VecDeque<u32>, v: u32, cap: usize) {
    if h.len() >= cap {
        h.pop_front();
    }
    h.push_back(v);
}

fn push_history_signed(h: &mut VecDeque<i32>, v: i32, cap: usize) {
    if h.len() >= cap {
        h.pop_front();
    }
    h.push_back(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_rates_come_from_deltas_and_first_sample_is_baseline() {
        let mut state = HardwareState::new();
        let s = |read_bytes, read_ops, read_time_ns| HwSample {
            interval: SAMPLE_INTERVAL,
            battery: None,
            system: None,
            disk: Some(DiskCounters {
                read_bytes,
                write_bytes: 0,
                read_ops,
                write_ops: 0,
                read_time_ns,
                write_time_ns: 0,
            }),
            volumes: Vec::new(),
            unmounted: None,
        };
        state.apply_sample(s(1_000_000, 100, 0));
        // Baseline: cumulative totals must not register as a rate.
        assert!(state.history_read.is_empty());
        state.apply_sample(s(3_000_000, 300, 100_000_000));
        // 2 MB over 2s = 1 MB/s; 200 ops over 2s = 100 IOPS; 0.5 ms mean.
        assert_eq!(state.history_read.back(), Some(&1_000_000));
        assert!((state.read_iops - 100.0).abs() < 0.001);
        assert!((state.read_lat_ms - 0.5).abs() < 0.001);
    }
}
