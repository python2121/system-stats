//! Linux backend for the Disk / Power tab. Everything here is readable
//! without root:
//!  - `/sys/class/power_supply/BAT*` — voltage, current, capacities,
//!    cycle count, and charge status, multiplied into the same live
//!    watts number the macOS backend derives from ioreg. The Apple-only
//!    extras (per-cell voltages, lifetime temperature extremes, USB-PD
//!    adapter profiles) have no sysfs equivalent and stay empty.
//!  - `/proc/diskstats` — cumulative sectors/ops/busy-time per block
//!    device. Whole disks only, so partition rows don't double-count.
//!  - sysinfo's disk list — per-volume capacity/fill.
//!  - `lsblk` — partitions that exist but aren't mounted.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use sysinfo::Disks;

use super::{BatterySnapshot, DiskCounters, SystemPowerSnapshot, UnmountedVolume, VolumeInfo};

pub fn battery() -> Option<BatterySnapshot> {
    let dir = find_supply("Battery")?;
    let on_ac = find_supply("Mains")
        .and_then(|d| read_trimmed(d.join("online")))
        .map(|v| v == "1");
    battery_from(&|name| read_trimmed(dir.join(name)), on_ac)
}

pub fn disk_counters() -> Option<DiskCounters> {
    fs::read_to_string("/proc/diskstats")
        .ok()
        .map(|t| parse_diskstats(&t))
}

// Unlike macOS' ~2s diskutil sweep this is nearly instant, but it keeps
// the same relaxed cadence — the partition table rarely changes.
pub fn scan_unmounted() -> Option<Vec<UnmountedVolume>> {
    let out = Command::new("lsblk")
        .args(["-rbn", "-o", "NAME,TYPE,FSTYPE,LABEL,SIZE,MOUNTPOINT"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(parse_lsblk(&String::from_utf8_lossy(&out.stdout)))
}

// Whole-package power/thermal telemetry from /sys/class/hwmon. The
// wattage comes from the amdgpu driver's PPT sensor: on an AMD APU
// (Steam Deck, most handhelds) that tracks the whole SoC package, on a
// discrete card (Steam Machine) just the GPU — covers_cpu records
// which. RAPL would add the CPU on any machine but its energy counters
// went root-only after the PLATYPUS side-channel, so machines without
// an amdgpu report None and the panel stays dark.
pub fn system_power() -> Option<SystemPowerSnapshot> {
    let mut snap = SystemPowerSnapshot::default();
    let mut have_watts = false;
    for entry in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = entry.path();
        let Some(name) = read_trimmed(dir.join("name")) else { continue };
        merge_hwmon(&mut snap, &mut have_watts, &name, &|f| {
            read_trimmed(dir.join(f))
        });
        // The hwmon's device symlink leads to the PCI device, whose
        // gpu_metrics blob tells APU from discrete card.
        if name == "amdgpu" && !snap.covers_cpu {
            if let Ok(blob) = fs::read(dir.join("device/gpu_metrics")) {
                snap.covers_cpu = gpu_metrics_is_apu(&blob);
            }
        }
    }
    have_watts.then_some(snap)
}

// gpu_metrics header: u16 size, u8 format_revision, u8 content_revision.
// Format 1 is the discrete-GPU table; 2 and 3 are the APU tables, whose
// socket power includes the CPU cores.
fn gpu_metrics_is_apu(blob: &[u8]) -> bool {
    blob.get(2).is_some_and(|rev| *rev >= 2)
}

// Fold one hwmon device into the snapshot. Factored over `read` (like
// battery_from) so tests can feed it maps instead of a /sys tree.
// First writer wins for every field: with two GPUs the first amdgpu is
// arbitrary but stable within a boot, and any device's spinning fan
// beats a later one's.
fn merge_hwmon(
    snap: &mut SystemPowerSnapshot,
    have_watts: &mut bool,
    name: &str,
    read: &dyn Fn(&str) -> Option<String>,
) {
    let num = |file: &str| read(file).and_then(|v| v.parse::<i64>().ok());
    match name {
        "amdgpu" => {
            // power1_average on most kernels; newer ones expose
            // power1_input instead. Both are µW.
            if !*have_watts {
                if let Some(uw) = num("power1_average").or_else(|| num("power1_input")) {
                    snap.package_watts = uw as f64 / 1e6;
                    *have_watts = true;
                    snap.cap_watts =
                        num("power1_cap").map(|uw| uw as f64 / 1e6).filter(|w| *w > 0.0);
                }
            }
            if snap.gpu_temp_c.is_none() {
                // temp1 is the edge sensor on amdgpu.
                snap.gpu_temp_c = num("temp1_input").map(|t| t as f64 / 1000.0);
            }
            if snap.gpu_mv.is_none() {
                // in0 is the vddgfx rail, already in millivolts.
                snap.gpu_mv = num("in0_input").filter(|v| *v > 0);
            }
        }
        // CPU die temperature: k10temp's temp1 is Tctl on AMD;
        // coretemp's is the package sensor on Intel.
        "k10temp" | "coretemp" | "zenpower" => {
            if snap.cpu_temp_c.is_none() {
                snap.cpu_temp_c = num("temp1_input").map(|t| t as f64 / 1000.0);
            }
        }
        _ => {}
    }
    // Any device's fan counts (the Deck/Steam Machine fan lives under
    // steamdeck_hwmon, not amdgpu), but only one that's actually
    // reporting — amdgpu often has a fan1_input stuck at 0.
    if snap.fan_rpm.is_none() {
        snap.fan_rpm = num("fan1_input").filter(|r| *r > 0);
    }
}

// First /sys/class/power_supply entry whose `type` matches — "Battery"
// for the pack, "Mains" for the AC adapter. Desktops simply have no
// Battery entry.
fn find_supply(kind: &str) -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let dir = entry.path();
        if read_trimmed(dir.join("type")).is_some_and(|t| t == kind) {
            return Some(dir);
        }
    }
    None
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

// Build a snapshot from sysfs attribute lookups. Factored over `read` so
// tests can feed it a map instead of a real /sys tree.
//
// Units per the power_supply ABI: voltages µV, currents µA, charge µAh,
// energy µWh, power µW, temp tenths of °C. Charge-reporting batteries
// expose charge_*; energy-reporting ones expose energy_* instead, which
// convert to mAh through the design voltage so health math works either
// way. Current is reported as a magnitude by most drivers with `status`
// carrying direction (and signed by a few) — abs + sign-by-status
// normalizes both to the macOS convention of negative-while-draining.
fn battery_from(
    read: &dyn Fn(&str) -> Option<String>,
    on_ac: Option<bool>,
) -> Option<BatterySnapshot> {
    let num = |name: &str| read(name).and_then(|v| v.parse::<i64>().ok());
    // No voltage ⇒ no usable battery, same as the macOS backend's
    // missing Voltage key.
    let voltage_mv = num("voltage_now")? / 1000;
    let status = read("status").unwrap_or_default();
    let discharging = status == "Discharging";

    let current_ma = num("current_now").map(|ua| (ua / 1000).abs()).unwrap_or(0);
    let amperage_ma = if discharging { -current_ma } else { current_ma };

    let mah = |charge_key: &str, energy_key: &str| -> i64 {
        if let Some(uah) = num(charge_key) {
            return uah / 1000;
        }
        match (num(energy_key), num("voltage_min_design").filter(|v| *v > 0)) {
            // µWh / µV = Ah, ×1000 = mAh.
            (Some(uwh), Some(uv)) => uwh * 1000 / uv,
            _ => 0,
        }
    };
    let design_capacity_mah = mah("charge_full_design", "energy_full_design");
    let max_capacity_mah = mah("charge_full", "energy_full");
    let current_capacity_mah = mah("charge_now", "energy_now");

    let watts_mag = match num("power_now") {
        Some(uw) => uw.abs() as f64 / 1e6,
        None => voltage_mv as f64 / 1000.0 * (current_ma as f64 / 1000.0),
    };
    let watts = if discharging { -watts_mag } else { watts_mag };

    // Kernel-provided estimates when the driver has them, otherwise the
    // naive capacity/current division.
    let time_to_empty_min = discharging
        .then(|| {
            num("time_to_empty_now")
                .map(|secs| secs / 60)
                .or_else(|| (current_ma > 0).then(|| current_capacity_mah * 60 / current_ma))
        })
        .flatten()
        .filter(|m| *m > 0);
    let time_to_full_min = (status == "Charging")
        .then(|| {
            num("time_to_full_now").map(|secs| secs / 60).or_else(|| {
                (current_ma > 0 && max_capacity_mah > current_capacity_mah)
                    .then(|| (max_capacity_mah - current_capacity_mah) * 60 / current_ma)
            })
        })
        .flatten()
        .filter(|m| *m > 0);

    Some(BatterySnapshot {
        percent: num("capacity").unwrap_or(0),
        voltage_mv,
        amperage_ma,
        watts,
        temperature_c: num("temp").map(|t| t as f64 / 10.0).unwrap_or(0.0),
        cycle_count: num("cycle_count").unwrap_or(0),
        design_capacity_mah,
        max_capacity_mah,
        current_capacity_mah,
        is_charging: status == "Charging",
        external_connected: on_ac.unwrap_or(status == "Charging" || status == "Full"),
        fully_charged: status == "Full",
        time_to_empty_min,
        time_to_full_min,
        // sysfs has no per-cell, daily-SoC, lifetime-extreme, or adapter
        // detail — those panels stay dark on Linux.
        cell_voltages_mv: Vec::new(),
        daily_min_soc: None,
        daily_max_soc: None,
        lifetime_temp_min_c: None,
        lifetime_temp_max_c: None,
        adapter: None,
    })
}

// /proc/diskstats: `major minor name` then per-device counters. Fields
// after the name: [0] reads completed, [2] sectors read, [3] ms reading,
// [4] writes completed, [6] sectors written, [7] ms writing. Sectors are
// always 512 bytes here regardless of the device's real sector size.
fn parse_diskstats(text: &str) -> DiskCounters {
    let mut c = DiskCounters::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 11 || !is_whole_disk(f[2]) {
            continue;
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        c.read_ops += n(3);
        c.read_bytes += n(5) * 512;
        c.read_time_ns += n(6) * 1_000_000;
        c.write_ops += n(7);
        c.write_bytes += n(9) * 512;
        c.write_time_ns += n(10) * 1_000_000;
    }
    c
}

// Physical whole-disk names: sda, vdb, xvda, nvme0n1, mmcblk0. Excludes
// their partitions (sda1, nvme0n1p1) so I/O isn't counted twice, and the
// virtual devices (loop, ram, zram, dm-, md, sr, fd) that either overlay
// a real disk or aren't one.
fn is_whole_disk(name: &str) -> bool {
    const VIRTUAL: [&str; 7] = ["loop", "ram", "zram", "dm-", "md", "sr", "fd"];
    if VIRTUAL.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    if let Some(rest) = name.strip_prefix("nvme").or(name.strip_prefix("mmcblk")) {
        // nvme0n1 / mmcblk0 are whole devices; 'p' introduces a partition.
        return !rest.contains('p');
    }
    // sd/vd/xvd/hd style: partitions append a digit to the disk name.
    !name.ends_with(|c: char| c.is_ascii_digit())
}

// Interesting volumes only: the root filesystem, a separate /home, and
// user-attached media under /mnt, /media, /run/media. Snap squashfs
// loops, /boot/efi, tmpfs and friends are noise.
pub fn read_volumes() -> Vec<VolumeInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut vols: Vec<VolumeInfo> = disks
        .list()
        .iter()
        .filter_map(|d| {
            let mount = d.mount_point().to_string_lossy().into_owned();
            let keep = mount == "/"
                || mount == "/home"
                || mount.starts_with("/mnt/")
                || mount.starts_with("/media/")
                || mount.starts_with("/run/media/");
            if !keep {
                return None;
            }
            let name = d.name().to_string_lossy().into_owned();
            Some(VolumeInfo {
                name: if name.is_empty() { mount.clone() } else { name },
                mount,
                total: d.total_space(),
                available: d.available_space(),
                removable: d.is_removable(),
            })
        })
        .collect();
    // Largest first; mount path settles ties so equal-sized volumes
    // don't swap rows between refreshes.
    vols.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.mount.cmp(&b.mount)));
    // Btrfs subvolumes (e.g. / and /home on one filesystem) share the
    // device and the space pool — collapse the twins like macOS does for
    // its system/Data pair. Sorting put them adjacent.
    vols.dedup_by(|a, b| a.name == b.name && a.total == b.total);
    vols
}

// One `lsblk -rbn` row per device: space-separated fields with specials
// hex-escaped (`\x20`), sizes in bytes, no header. An empty trailing
// MOUNTPOINT may or may not leave a trailing space, hence the get(5).
fn parse_lsblk(text: &str) -> Vec<UnmountedVolume> {
    let mut out: Vec<UnmountedVolume> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() < 5 {
            continue;
        }
        let (name, typ, fstype, label, size) = (f[0], f[1], f[2], f[3], f[4]);
        let mount = f.get(5).copied().unwrap_or("");
        if typ != "part" || !mount.is_empty() {
            continue; // whole disks, and anything already mounted
        }
        // Swap is in use despite having no mountpoint; LUKS/LVM/RAID
        // members surface through the volume they back.
        if matches!(fstype, "swap" | "crypto_LUKS" | "LVM2_member" | "linux_raid_member") {
            continue;
        }
        let size: u64 = size.parse().unwrap_or(0);
        if size == 0 {
            continue;
        }
        let kind = if fstype.is_empty() { "unknown".to_string() } else { fstype.to_string() };
        let label = unescape(label);
        out.push(UnmountedVolume {
            name: if label.is_empty() { kind.clone() } else { label },
            device: name.to_string(),
            kind,
            size,
        });
    }
    // Largest first, matching the mounted list's ordering.
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.device.cmp(&b.device)));
    out
}

// Undo lsblk -r's \xNN hex escapes (labels with spaces are the common
// case). Escapes are always two well-formed hex digits; bytes ≥ 0x80
// would need real UTF-8 reassembly, but ASCII covers labels in practice.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' || chars.peek() != Some(&'x') {
            out.push(c);
            continue;
        }
        chars.next(); // consume 'x'
        let hi = chars.next().and_then(|c| c.to_digit(16));
        let lo = chars.next().and_then(|c| c.to_digit(16));
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8 as char),
            _ => out.push_str("\\x"), // malformed — keep something visible
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn battery(files: &[(&str, &str)], on_ac: Option<bool>) -> Option<BatterySnapshot> {
        let map: HashMap<&str, &str> = files.iter().copied().collect();
        battery_from(&|name| map.get(name).map(|v| v.to_string()), on_ac)
    }

    #[test]
    fn charge_reporting_battery_discharging() {
        // Mirrors the macOS test fixture: 12.119 V, 2.878 A draining.
        let b = battery(
            &[
                ("voltage_now", "12119000"),
                ("current_now", "2878000"),
                ("status", "Discharging"),
                ("capacity", "73"),
                ("cycle_count", "319"),
                ("charge_full_design", "4563000"),
                ("charge_full", "3955000"),
                ("charge_now", "2887000"),
                ("temp", "310"),
            ],
            Some(false),
        )
        .unwrap();
        assert_eq!(b.voltage_mv, 12119);
        assert_eq!(b.amperage_ma, -2878);
        assert!((b.watts - (-34.88)).abs() < 0.1);
        assert_eq!(b.percent, 73);
        assert_eq!(b.cycle_count, 319);
        assert!((b.health_percent() - 86.67).abs() < 0.1);
        assert!((b.temperature_c - 31.0).abs() < 0.001);
        // 2887 mAh at 2878 mA ≈ 60 minutes left.
        assert_eq!(b.time_to_empty_min, Some(60));
        assert_eq!(b.time_to_full_min, None);
        assert!(!b.external_connected && !b.is_charging);
    }

    #[test]
    fn energy_reporting_battery_full_on_ac() {
        // ThinkPad-style: energy_* in µWh, converted via design voltage.
        let b = battery(
            &[
                ("voltage_now", "12800000"),
                ("status", "Full"),
                ("capacity", "100"),
                ("energy_full_design", "57000000"),
                ("energy_full", "50000000"),
                ("energy_now", "50000000"),
                ("voltage_min_design", "11400000"),
            ],
            Some(true),
        )
        .unwrap();
        // 57 Wh / 11.4 V = 5000 mAh design.
        assert_eq!(b.design_capacity_mah, 5000);
        assert_eq!(b.max_capacity_mah, 4385);
        assert!(b.fully_charged && b.external_connected && !b.is_charging);
        assert_eq!(b.watts, 0.0);
        assert_eq!(b.time_to_empty_min, None);
    }

    #[test]
    fn no_voltage_means_no_battery() {
        assert!(battery(&[("status", "Unknown")], None).is_none());
    }

    fn merge(snap: &mut SystemPowerSnapshot, have: &mut bool, name: &str, files: &[(&str, &str)]) {
        let map: HashMap<&str, &str> = files.iter().copied().collect();
        merge_hwmon(snap, have, name, &|f| map.get(f).map(|v| v.to_string()));
    }

    #[test]
    fn hwmon_devices_fold_into_one_snapshot() {
        // Steam Machine-shaped fixture: fan on the platform device,
        // power/temps on amdgpu (with a dead fan tach), CPU on k10temp.
        let mut snap = SystemPowerSnapshot::default();
        let mut have = false;
        merge(&mut snap, &mut have, "steamdeck_hwmon", &[("fan1_input", "481")]);
        merge(
            &mut snap,
            &mut have,
            "amdgpu",
            &[
                ("power1_average", "8000000"),
                ("power1_cap", "110000000"),
                ("temp1_input", "42000"),
                ("in0_input", "736"),
                ("fan1_input", "0"),
            ],
        );
        merge(&mut snap, &mut have, "k10temp", &[("temp1_input", "40625")]);
        assert!(have);
        assert!((snap.package_watts - 8.0).abs() < 0.001);
        assert_eq!(snap.cap_watts, Some(110.0));
        assert_eq!(snap.gpu_temp_c, Some(42.0));
        assert_eq!(snap.cpu_temp_c, Some(40.625));
        assert_eq!(snap.gpu_mv, Some(736));
        // The platform fan won and amdgpu's stuck-at-zero tach didn't
        // overwrite it.
        assert_eq!(snap.fan_rpm, Some(481));
    }

    #[test]
    fn gpu_metrics_header_tells_apu_from_discrete() {
        // v1.3 header from a real Navi 33 (discrete).
        assert!(!gpu_metrics_is_apu(&[0x78, 0x00, 0x01, 0x03]));
        // v2.x / v3.x are the APU tables.
        assert!(gpu_metrics_is_apu(&[0x88, 0x00, 0x02, 0x02]));
        assert!(gpu_metrics_is_apu(&[0x40, 0x00, 0x03, 0x00]));
        // Unreadable/truncated blob: keep the conservative GPU-only label.
        assert!(!gpu_metrics_is_apu(&[]));
    }

    #[test]
    fn no_power_sensor_means_no_snapshot() {
        // Temps alone don't make a snapshot — the chart needs watts.
        let mut snap = SystemPowerSnapshot::default();
        let mut have = false;
        merge(&mut snap, &mut have, "coretemp", &[("temp1_input", "50000")]);
        assert!(!have);
        // power1_input is the fallback spelling for the wattage.
        merge(&mut snap, &mut have, "amdgpu", &[("power1_input", "15500000")]);
        assert!(have);
        assert!((snap.package_watts - 15.5).abs() < 0.001);
        assert_eq!(snap.cap_watts, None);
    }

    #[test]
    fn whole_disk_detection() {
        for whole in ["sda", "vdb", "xvda", "hda", "nvme0n1", "mmcblk0", "nvme10n2"] {
            assert!(is_whole_disk(whole), "{whole} should be whole");
        }
        for not in ["sda1", "nvme0n1p1", "mmcblk0p2", "loop0", "ram0", "zram0", "dm-0", "md127", "sr0", "fd0"] {
            assert!(!is_whole_disk(not), "{not} should be excluded");
        }
    }

    // Trimmed from a real /proc/diskstats: an NVMe disk, its partition
    // (would double-count), a loop device, and a SATA disk.
    const DISKSTATS: &str = "\
 259       0 nvme0n1 121466 51 7565378 12080 273265 108086 19870341 176571 0 91700 188652 0 0 0 0 0 0
 259       1 nvme0n1p1 355 0 21266 26 2 0 2 0 0 40 26 0 0 0 0 0 0
   7       0 loop0 55 0 2148 12 0 0 0 0 0 20 12 0 0 0 0 0 0
   8       0 sda 1000 0 2000 30 500 0 4000 70 0 100 100 0 0 0 0 0 0
";

    #[test]
    fn diskstats_sums_whole_disks_only() {
        let d = parse_diskstats(DISKSTATS);
        assert_eq!(d.read_ops, 121466 + 1000);
        assert_eq!(d.read_bytes, (7565378 + 2000) * 512);
        assert_eq!(d.read_time_ns, (12080 + 30) * 1_000_000);
        assert_eq!(d.write_ops, 273265 + 500);
        assert_eq!(d.write_bytes, (19870341 + 4000) * 512);
        assert_eq!(d.write_time_ns, (176571 + 70) * 1_000_000);
    }

    // Hand-built `lsblk -rbn` output: a whole disk, a mounted EFI
    // partition, the mounted root, swap, an unmounted NTFS volume with an
    // escaped space in its label, and a bare unformatted partition.
    const LSBLK: &str = "\
nvme0n1 disk   512110190592
nvme0n1p1 part vfat  536870912 /boot/efi
nvme0n1p2 part ext4 root 511101108224 /
nvme0n1p3 part swap  8589934592
sda1 part ntfs Windows\\x20Data 240057409536
sda2 part   1048576
";

    #[test]
    fn lsblk_finds_unmounted_partitions_and_skips_noise() {
        let vols = parse_lsblk(LSBLK);
        let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["Windows Data", "unknown"]);
        assert_eq!(vols[0].device, "sda1");
        assert_eq!(vols[0].kind, "ntfs");
        assert_eq!(vols[0].size, 240057409536);
        assert_eq!(vols[1].device, "sda2");
    }

    #[test]
    fn unescape_handles_hex_and_literals() {
        assert_eq!(unescape("Windows\\x20Data"), "Windows Data");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("trailing\\x"), "trailing\\x");
    }
}
