//! macOS backend for the Disk / Power tab. Everything here is readable
//! without root:
//!  - `ioreg -rn AppleSmartBattery` — pack voltage and *signed* amperage
//!    (negative = discharging), which multiply into a live watts number
//!    macOS itself doesn't surface anywhere in the UI. Plus per-cell
//!    voltages, temperature, cycle count, health capacities, lifetime
//!    temperature extremes, and the negotiated USB-PD adapter profile.
//!  - `ioreg -rc IOBlockStorageDriver` — cumulative read/write bytes,
//!    operation counts, and busy-time per storage driver. Diffing two
//!    samples gives throughput, IOPS, and average I/O latency.
//!  - sysinfo's disk list — per-volume capacity/fill.
//!  - `diskutil info -all` — partitions that exist but aren't mounted.

use std::process::Command;

use sysinfo::Disks;

use super::{AdapterInfo, BatterySnapshot, DiskCounters, UnmountedVolume, VolumeInfo};

pub fn battery() -> Option<BatterySnapshot> {
    ioreg(&["-rn", "AppleSmartBattery", "-w0"]).and_then(|t| parse_battery(&t))
}

pub fn disk_counters() -> Option<DiskCounters> {
    ioreg(&["-rc", "IOBlockStorageDriver", "-w0"]).map(|t| parse_disk_counters(&t))
}

// The slow sweep: `diskutil info -all` takes ~2s, which is why the
// caller runs this on its own thread at a relaxed cadence.
pub fn scan_unmounted() -> Option<Vec<UnmountedVolume>> {
    Command::new("diskutil")
        .args(["info", "-all"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_unmounted(&String::from_utf8_lossy(&o.stdout)))
}

fn ioreg(args: &[&str]) -> Option<String> {
    let out = Command::new("ioreg").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Interesting volumes only: the system root, the user data volume, and
// anything mounted under /Volumes. The half-dozen tiny APFS bookkeeping
// volumes (Preboot, VM, Update, …) are noise.
pub fn read_volumes() -> Vec<VolumeInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut vols: Vec<VolumeInfo> = disks
        .list()
        .iter()
        .filter_map(|d| {
            let mount = d.mount_point().to_string_lossy().into_owned();
            let keep = mount == "/"
                || mount == "/System/Volumes/Data"
                || mount.starts_with("/Volumes/");
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
    // The system and Data volumes share one APFS container, and space
    // accounting is container-level — the rows would be identical twins.
    // Keep just the root row.
    if let Some(root) = vols.iter().position(|v| v.mount == "/") {
        if let Some(data) = vols.iter().position(|v| v.mount == "/System/Volumes/Data") {
            let keep_root = root.min(data);
            vols.remove(root.max(data));
            vols[keep_root].mount = "/".to_string();
        }
    }
    vols
}

// APFS bookkeeping volumes that would otherwise show up as "unmounted"
// noise on every Mac.
const SYSTEM_VOLUME_NAMES: [&str; 8] = [
    "Preboot", "Recovery", "VM", "Update", "xART", "xarts", "iSCPreboot", "Hardware",
];

// Pull unmounted, user-relevant partitions out of `diskutil info -all`.
// The output is a run of key/value blocks, one per device, separated by
// each block's leading "Device Identifier" key.
fn parse_unmounted(text: &str) -> Vec<UnmountedVolume> {
    let mut out: Vec<UnmountedVolume> = Vec::new();
    let mut device = String::new();
    let mut whole = false;
    let mut name = String::new();
    let mut mounted = String::new();
    let mut ptype = String::new();
    let mut fs = String::new();
    let mut size: u64 = 0;

    let mut flush = |device: &str, whole: bool, name: &str, mounted: &str, ptype: &str, fs: &str, size: u64| {
        if device.is_empty() || whole || mounted.starts_with("Yes") {
            return; // not a partition, or already in the mounted list
        }
        // APFS container stores and their recovery/ISC siblings are
        // backing partitions — their contents surface as synthesized
        // volumes, so listing them too would double-count the disk.
        if ptype.starts_with("Apple_APFS") {
            return;
        }
        let named = !name.is_empty() && !name.starts_with("Not applicable");
        if named && SYSTEM_VOLUME_NAMES.iter().any(|s| name == *s) {
            return;
        }
        if size == 0 {
            return;
        }
        let display = if named { name } else { ptype };
        if display.is_empty() {
            return;
        }
        let kind = if fs.is_empty() { ptype } else { fs };
        out.push(UnmountedVolume {
            name: display.to_string(),
            device: device.to_string(),
            kind: kind.to_string(),
            size,
        });
    };

    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "Device Identifier" => {
                // New block: evaluate the previous one first.
                flush(&device, whole, &name, &mounted, &ptype, &fs, size);
                device = value.to_string();
                whole = false;
                name = String::new();
                mounted = String::new();
                ptype = String::new();
                fs = String::new();
                size = 0;
            }
            "Whole" => whole = value == "Yes",
            "Volume Name" => name = value.to_string(),
            "Mounted" => mounted = value.to_string(),
            "Partition Type" => ptype = value.to_string(),
            "File System Personality" => fs = value.to_string(),
            "Disk Size" => {
                // "32.7 GB (32667336704 Bytes) (exactly …)" — take the
                // exact byte count from the first parenthetical.
                size = value
                    .split('(')
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    flush(&device, whole, &name, &mounted, &ptype, &fs, size);
    // Largest first, matching the mounted list's ordering.
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.device.cmp(&b.device)));
    out
}

// ---------- ioreg text scanning ----------
//
// ioreg's plain-text output mixes ` "Key" = value ` (top level) with
// `"Key"=value` (inside nested dicts). Scanning for the quoted key and
// then skipping spaces/`=` handles both without a real parser.

// First integer following `"key"`. ioreg prints negative numbers as
// wrapped u64s (e.g. Amperage = 18446744073709548738 ≡ -2878), so parse
// wide and reinterpret.
fn find_num(text: &str, key: &str) -> Option<i64> {
    let at = find_value_start(text, key, 0)?;
    parse_int(&text[at..])
}

// Every integer following any occurrence of `"key"` — for counters that
// repeat once per storage driver.
fn find_nums(text: &str, key: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_value_start(text, key, from) {
        if let Some(v) = parse_int(&text[at..]) {
            out.push(v);
        }
        from = at;
    }
    out
}

fn find_bool(text: &str, key: &str) -> Option<bool> {
    let at = find_value_start(text, key, 0)?;
    let rest = text[at..].trim_start();
    if rest.starts_with("Yes") {
        Some(true)
    } else if rest.starts_with("No") {
        Some(false)
    } else {
        None
    }
}

// Parenthesized list of integers: `"CellVoltage"=(4342,4343,4343)`.
fn find_num_list(text: &str, key: &str) -> Vec<i64> {
    let Some(at) = find_value_start(text, key, 0) else { return Vec::new() };
    let rest = &text[at..];
    let Some(open) = rest.find('(') else { return Vec::new() };
    let Some(close) = rest[open..].find(')') else { return Vec::new() };
    rest[open + 1..open + close]
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

// Byte offset just past `"key"` and any ` = ` separator, or None if the
// key never appears (search starts at `from`).
fn find_value_start(text: &str, key: &str, from: usize) -> Option<usize> {
    let pat = format!("\"{key}\"");
    let hit = text[from..].find(&pat)? + from + pat.len();
    let skipped = text[hit..]
        .char_indices()
        .find(|(_, c)| *c != ' ' && *c != '=')
        .map(|(i, _)| i)
        .unwrap_or(0);
    Some(hit + skipped)
}

fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let raw: u64 = s[..end].parse().ok()?;
    // Wrapped-negative u64 (two's complement) → the i64 it really is.
    let v = raw as i64;
    Some(if neg { -v } else { v })
}

// 65535 is ioreg's "no valid estimate" sentinel for the time fields.
fn valid_minutes(v: Option<i64>) -> Option<i64> {
    v.filter(|m| *m > 0 && *m < 65535)
}

fn parse_battery(text: &str) -> Option<BatterySnapshot> {
    // No Voltage key ⇒ no battery on this machine.
    let voltage_mv = find_num(text, "Voltage")?;
    let amperage_ma = find_num(text, "Amperage").unwrap_or(0);
    let external_connected = find_bool(text, "ExternalConnected").unwrap_or(false);

    let adapter = match (external_connected, find_num(text, "Watts")) {
        (true, Some(watts)) if watts > 0 => Some(AdapterInfo {
            watts,
            voltage_mv: find_num(text, "AdapterVoltage").unwrap_or(0),
            current_ma: find_num(text, "Current").unwrap_or(0),
            is_wireless: find_bool(text, "IsWireless").unwrap_or(false),
            profile_volts: {
                let mut v: Vec<i64> = find_nums(text, "MaxVoltage")
                    .into_iter()
                    .map(|mv| mv / 1000)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            },
        }),
        _ => None,
    };

    Some(BatterySnapshot {
        percent: find_num(text, "CurrentCapacity").unwrap_or(0),
        voltage_mv,
        amperage_ma,
        watts: voltage_mv as f64 / 1000.0 * (amperage_ma as f64 / 1000.0),
        temperature_c: find_num(text, "Temperature").unwrap_or(0) as f64 / 100.0,
        cycle_count: find_num(text, "CycleCount").unwrap_or(0),
        design_capacity_mah: find_num(text, "DesignCapacity").unwrap_or(0),
        max_capacity_mah: find_num(text, "AppleRawMaxCapacity").unwrap_or(0),
        current_capacity_mah: find_num(text, "AppleRawCurrentCapacity").unwrap_or(0),
        is_charging: find_bool(text, "IsCharging").unwrap_or(false),
        external_connected,
        fully_charged: find_bool(text, "FullyCharged").unwrap_or(false),
        time_to_empty_min: valid_minutes(find_num(text, "AvgTimeToEmpty")),
        time_to_full_min: valid_minutes(find_num(text, "AvgTimeToFull")),
        cell_voltages_mv: find_num_list(text, "CellVoltage"),
        daily_min_soc: find_num(text, "DailyMinSoc"),
        daily_max_soc: find_num(text, "DailyMaxSoc"),
        lifetime_temp_min_c: find_num(text, "MinimumTemperature"),
        lifetime_temp_max_c: find_num(text, "MaximumTemperature"),
        adapter,
    })
}

// Sum the cumulative Statistics counters across every storage driver
// (internal SSD + any externals).
fn parse_disk_counters(text: &str) -> DiskCounters {
    let sum = |key: &str| find_nums(text, key).iter().map(|v| *v as u64).sum();
    DiskCounters {
        read_bytes: sum("Bytes (Read)"),
        write_bytes: sum("Bytes (Write)"),
        read_ops: sum("Operations (Read)"),
        write_ops: sum("Operations (Write)"),
        read_time_ns: sum("Total Time (Read)"),
        write_time_ns: sum("Total Time (Write)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from real `ioreg -rn AppleSmartBattery -w0` output on a
    // MacBook Air (M2), discharging at 2.878 A. Note the wrapped-u64
    // Amperage and the nested BatteryData/AdapterDetails dicts.
    const BATTERY_DISCHARGING: &str = r#"
      "AppleRawAdapterDetails" = ({"IsWireless"=No,"AdapterID"=0,"FamilyCode"=0})
      "CurrentCapacity" = 73
      "TimeRemaining" = 262
      "Amperage" = 18446744073709548738
      "AppleRawCurrentCapacity" = 2887
      "AvgTimeToFull" = 65535
      "ExternalConnected" = No
      "BatteryData" = {"CellVoltage"=(4042,4043,4041),"DailyMinSoc"=61,"DailyMaxSoc"=100,"DesignCapacity"=4563,"CycleCount"=319,"Voltage"=12119,"LifetimeData"={"MinimumTemperature"=7,"MaximumTemperature"=39}}
      "NominalChargeCapacity" = 4082
      "FullyCharged" = No
      "MaxCapacity" = 100
      "Temperature" = 3099
      "AvgTimeToEmpty" = 262
      "IsCharging" = No
      "DesignCapacity" = 4563
      "Voltage" = 12119
      "CycleCount" = 319
      "AppleRawMaxCapacity" = 3955
"#;

    const BATTERY_ON_AC: &str = r#"
      "AppleRawAdapterDetails" = ({"IsWireless"=No,"AdapterVoltage"=20000,"Watts"=60,"Current"=3000,"UsbHvcMenu"=({"Index"=0,"MaxCurrent"=3000,"MaxVoltage"=5000},{"Index"=4,"MaxCurrent"=3000,"MaxVoltage"=20000})})
      "CurrentCapacity" = 100
      "Amperage" = 0
      "ExternalConnected" = Yes
      "IsCharging" = No
      "FullyCharged" = Yes
      "AvgTimeToEmpty" = 65535
      "AvgTimeToFull" = 65535
      "Temperature" = 3050
      "DesignCapacity" = 4563
      "Voltage" = 13027
      "CycleCount" = 319
      "AppleRawMaxCapacity" = 3955
      "AppleRawCurrentCapacity" = 3955
"#;

    const DISK_TWO_DRIVERS: &str = r#"
  | "Statistics" = {"Operations (Write)"=100,"Bytes (Read)"=4096,"Errors (Write)"=0,"Total Time (Read)"=2000000,"Total Time (Write)"=1000000,"Bytes (Write)"=8192,"Operations (Read)"=200}
  | "Statistics" = {"Operations (Write)"=1,"Bytes (Read)"=1000,"Total Time (Read)"=500,"Total Time (Write)"=500,"Bytes (Write)"=2000,"Operations (Read)"=2}
"#;

    #[test]
    fn parses_discharging_battery_with_wrapped_negative_amperage() {
        let b = parse_battery(BATTERY_DISCHARGING).unwrap();
        assert_eq!(b.amperage_ma, -2878);
        assert_eq!(b.voltage_mv, 12119);
        // 12.119 V × -2.878 A ≈ -34.9 W
        assert!((b.watts - (-34.88)).abs() < 0.1);
        assert_eq!(b.percent, 73);
        assert_eq!(b.cycle_count, 319);
        assert_eq!(b.cell_voltages_mv, vec![4042, 4043, 4041]);
        assert_eq!(b.time_to_empty_min, Some(262));
        assert_eq!(b.time_to_full_min, None);
        assert_eq!(b.daily_min_soc, Some(61));
        assert_eq!(b.lifetime_temp_max_c, Some(39));
        assert!((b.temperature_c - 30.99).abs() < 0.001);
        assert!((b.health_percent() - 86.67).abs() < 0.1);
        // On battery — no adapter even though the (stale) details dict exists.
        assert!(b.adapter.is_none());
    }

    #[test]
    fn parses_adapter_when_on_ac() {
        let b = parse_battery(BATTERY_ON_AC).unwrap();
        assert_eq!(b.watts, 0.0);
        assert!(b.fully_charged && b.external_connected && !b.is_charging);
        let a = b.adapter.unwrap();
        assert_eq!(a.watts, 60);
        assert_eq!(a.voltage_mv, 20000);
        assert_eq!(a.current_ma, 3000);
        assert_eq!(a.profile_volts, vec![5, 20]);
    }

    #[test]
    fn no_battery_key_means_no_battery() {
        assert!(parse_battery("\"SomeOtherService\" = 1").is_none());
    }

    #[test]
    fn sums_disk_counters_across_drivers() {
        let d = parse_disk_counters(DISK_TWO_DRIVERS);
        assert_eq!(d.read_bytes, 5096);
        assert_eq!(d.write_bytes, 10192);
        assert_eq!(d.read_ops, 202);
        assert_eq!(d.write_ops, 101);
        assert_eq!(d.read_time_ns, 2000500);
    }

    // Trimmed from real `diskutil info -all` output on this machine
    // (Asahi Linux dual-boot): a whole disk, an APFS container store, a
    // named EFI partition, an ext4 root, a mounted APFS volume, and an
    // APFS bookkeeping volume.
    const DISKUTIL_ALL: &str = "\
   Device Identifier:         disk0
   Whole:                     Yes
   Volume Name:               Not applicable (no file system)
   Mounted:                   Not applicable (no file system)
   Disk Size:                 251.0 GB (251000193024 Bytes) (exactly 490234752 512-Byte-Units)

   Device Identifier:         disk0s2
   Whole:                     No
   Volume Name:               Not applicable (no file system)
   Mounted:                   Not applicable (no file system)
   Partition Type:            Apple_APFS
   Disk Size:                 208.3 GB (208341565440 Bytes) (exactly 406917120 512-Byte-Units)

   Device Identifier:         disk0s4
   Whole:                     No
   Volume Name:               EFI - FEDOR
   Mounted:                   No
   Partition Type:            EFI
   File System Personality:   MS-DOS FAT32
   Disk Size:                 524.3 MB (524288000 Bytes) (exactly 1024000 512-Byte-Units)

   Device Identifier:         disk0s6
   Whole:                     No
   Volume Name:               Not applicable (no file system)
   Mounted:                   Not applicable (no file system)
   Partition Type:            Linux Filesystem
   Disk Size:                 32.7 GB (32667336704 Bytes) (exactly 63803392 512-Byte-Units)

   Device Identifier:         disk1s2
   Whole:                     No
   Volume Name:               Fedora
   Mounted:                   Yes
   Partition Type:            41504653-0000-11AA-AA11-00306543ECAC
   File System Personality:   APFS
   Disk Size:                 2.5 GB (2499805184 Bytes) (exactly 4882432 512-Byte-Units)

   Device Identifier:         disk1s3
   Whole:                     No
   Volume Name:               Preboot
   Mounted:                   No
   Partition Type:            41504653-0000-11AA-AA11-00306543ECAC
   File System Personality:   APFS
   Disk Size:                 2.5 GB (2499805184 Bytes) (exactly 4882432 512-Byte-Units)
";

    #[test]
    fn finds_unmounted_partitions_and_skips_noise() {
        let vols = parse_unmounted(DISKUTIL_ALL);
        let names: Vec<&str> = vols.iter().map(|v| v.name.as_str()).collect();
        // The Linux root (no macOS-readable fs → named by partition type)
        // and the named EFI partition are in, largest first; the whole
        // disk, the APFS container store, the mounted volume, and
        // Preboot are out.
        assert_eq!(names, vec!["Linux Filesystem", "EFI - FEDOR"]);
        let linux = &vols[0];
        assert_eq!(linux.device, "disk0s6");
        assert_eq!(linux.size, 32667336704);
        assert_eq!(linux.kind, "Linux Filesystem");
        assert_eq!(vols[1].kind, "MS-DOS FAT32");
    }
}
