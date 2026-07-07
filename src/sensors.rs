//! Battery / CPU / GPU sensor readouts and their cached state.
//!
//! The power_supply, RAPL, and thermal sysfs attributes go through the SMC on
//! T2 Macs and can take tens of ms to read, so they are never touched on the
//! render path -- poller threads read them and stash the latest value in the
//! statics below, which the render path reads without blocking.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

/// Latest battery reading (capacity %, state), updated by a poller thread.
/// The power_supply sysfs attributes go through the SMC on T2 Macs and can
/// take tens of ms to read, so they must never be touched on the render path
/// -- that was a visible frame hitch on every battery refresh.
pub(crate) static BATTERY_STATE: Mutex<(u32, BatteryState)> =
    Mutex::new((100, BatteryState::NotCharging));

/// Latest CPU temperature in whole °C, updated by a poller thread (same
/// never-read-sysfs-on-the-render-path rule as BATTERY_STATE). `None` means no
/// readable thermal zone.
pub(crate) static CPU_TEMP_STATE: Mutex<Option<i32>> = Mutex::new(None);
/// Latest CPU package power draw in whole watts, derived from the RAPL energy
/// counter by a poller thread. `None` means no readable RAPL counter.
pub(crate) static CPU_POWER_STATE: Mutex<Option<i32>> = Mutex::new(None);

/// Latest GPU temperature and package power (whole °C / whole W), updated by a
/// poller thread (same never-read-sysfs-on-the-render-path rule as the CPU
/// states). `None` means the detected GPU doesn't expose that sensor.
pub(crate) static GPU_TEMP_STATE: Mutex<Option<i32>> = Mutex::new(None);
pub(crate) static GPU_POWER_STATE: Mutex<Option<i32>> = Mutex::new(None);
/// The detected GPU's vendor label ("AMD"/"NVIDIA"/"Intel"), or a bare "GPU"
/// until detection resolves it. Shown as the widget's label prefix.
pub(crate) static GPU_LABEL: Mutex<&'static str> = Mutex::new("GPU");

pub(crate) fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path).unwrap_or_else(|_| "Unknown".to_string());

    // Prefer charge_now/charge_full, but mirror UPower's recalibration: on T2
    // Macs (and degraded batteries generally) the SMC's `charge_full` is a bad
    // learned value that lags and can sit *below* charge_now just after a full
    // charge (e.g. now=5627000, full=4240000). The kernel's own `capacity` is
    // no help here -- on this hardware it is computed against charge_full_design
    // (the pristine design capacity), so a battery degraded to ~77% health caps
    // out near 77% and never reads full even when it is. Treat the effective
    // full as max(charge_full, charge_now) so a stale-low charge_full snaps up
    // to charge_now and reports 100% at full charge, matching UPower/caelestia.
    // Fall back to `capacity` only when charge_now/charge_full aren't exposed
    // (e.g. Apple Silicon, which reports capacity directly).
    let read_uah = |attr: &str| {
        let path = format!("/sys/class/power_supply/{}/{}", battery, attr);
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
    };
    let capacity = match (read_uah("charge_now"), read_uah("charge_full")) {
        (Some(now), Some(full)) if now > 0.0 => {
            let effective_full = full.max(now);
            ((now / effective_full) * 100.0).round().clamp(0.0, 100.0) as u32
        }
        _ => {
            let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
            fs::read_to_string(&capacity_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|c| c.min(100))
                .unwrap_or(100)
        }
    };

    let status = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, status)
}

/// Open the Intel RAPL package energy counter. `energy_uj` is root-only (a
/// power side-channel mitigation), so this must be called *before* the daemon
/// drops privileges -- the poller then reads the already-open fd, since DAC is
/// checked at open() not per read. Returns the open file and the counter's
/// wrap-around range in µJ. CPU package power is the delta of this over time.
pub(crate) fn open_cpu_power_source() -> Option<(File, u64)> {
    for entry in fs::read_dir("/sys/class/powercap").ok()?.flatten() {
        let path = entry.path();
        if !fs::read_to_string(path.join("name")).is_ok_and(|n| n.trim() == "package-0") {
            continue;
        }
        let file = File::open(path.join("energy_uj")).ok()?;
        let max = fs::read_to_string(path.join("max_energy_range_uj"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        return Some((file, max));
    }
    None
}

/// Read the current µJ value from an already-open RAPL `energy_uj` fd. sysfs
/// regenerates the value on each read from offset 0, so seek back first.
pub(crate) fn read_energy_uj(file: &mut File) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

/// The `temp` file of the x86 package-temperature thermal zone, when one
/// exists. Its absence selects the hottest-zone fallback in `read_cpu_temp`.
pub(crate) fn find_cpu_temp_zone() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let path = entry.path();
        if fs::read_to_string(path.join("type")).is_ok_and(|t| t.trim() == "x86_pkg_temp") {
            return Some(path.join("temp"));
        }
    }
    None
}

/// Read the CPU/SoC temperature in whole °C: the x86 package sensor when
/// present (`zone`), otherwise the hottest thermal zone (e.g. Apple Silicon).
pub(crate) fn read_cpu_temp(zone: Option<&Path>) -> Option<i32> {
    fn read_millideg(path: &Path) -> Option<i64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }
    let millideg = match zone {
        Some(path) => read_millideg(path),
        None => fs::read_dir("/sys/class/thermal")
            .ok()?
            .flatten()
            .filter_map(|e| read_millideg(&e.path().join("temp")))
            .max(),
    }?;
    Some((millideg / 1000) as i32)
}
