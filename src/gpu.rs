//! GPU sensor readout for the `Gpu` widget. Detects whether the machine's GPU
//! is AMD, NVIDIA, or Intel and picks the right way to read its temperature and
//! power: amdgpu/i915 expose both through hwmon sysfs, while NVIDIA's
//! proprietary stack has no stable hwmon and is read via `nvidia-smi`.
//!
//! All of these are readable unprivileged, so -- unlike the CPU's root-only RAPL
//! counter -- the poller needs no pre-drop file handle.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Which vendor made the detected GPU. Doubles as the widget's default label.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
}

impl GpuVendor {
    /// The label the `Gpu` widget shows when `GpuLabel` isn't disabled.
    pub fn label(self) -> &'static str {
        match self {
            GpuVendor::Amd => "AMD",
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Intel => "Intel",
        }
    }

    fn from_pci_id(raw: &str) -> Option<GpuVendor> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0x1002" => Some(GpuVendor::Amd),
            "0x10de" => Some(GpuVendor::Nvidia),
            "0x8086" => Some(GpuVendor::Intel),
            _ => None,
        }
    }
}

/// A GPU reading: temperature in whole °C and power draw in whole watts. Either
/// may be absent when the card doesn't expose that sensor.
#[derive(Copy, Clone, Default, PartialEq)]
pub struct GpuReading {
    pub temp: Option<i32>,
    pub watts: Option<i32>,
}

/// A detected GPU together with how to read its sensors.
pub struct Gpu {
    pub vendor: GpuVendor,
    source: GpuSource,
}

enum GpuSource {
    /// AMD/Intel: `temp1_input` (millidegrees) and, on cards that report it,
    /// `power1_average`/`power1_input` (microwatts) under the card's hwmon dir.
    Hwmon {
        temp: Option<PathBuf>,
        power: Option<PathBuf>,
    },
    /// NVIDIA: query the proprietary tool, which prints temp and power in one go.
    NvidiaSmi,
}

impl Gpu {
    /// Look for a usable GPU. Prefers a discrete card (AMD/NVIDIA) over Intel
    /// integrated graphics, so a laptop with both reports the interesting one.
    pub fn detect() -> Option<Gpu> {
        let mut candidates: Vec<Gpu> = Vec::new();
        for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
            // Only whole cards ("card0"), not connectors ("card0-eDP-1").
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("card") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let dev = entry.path().join("device");
            let Some(vendor) = fs::read_to_string(dev.join("vendor"))
                .ok()
                .and_then(|raw| GpuVendor::from_pci_id(&raw))
            else {
                continue;
            };
            let source = match vendor {
                GpuVendor::Nvidia => nvidia_smi_available().then_some(GpuSource::NvidiaSmi),
                GpuVendor::Amd | GpuVendor::Intel => {
                    find_hwmon(&dev).map(|(temp, power)| GpuSource::Hwmon { temp, power })
                }
            };
            if let Some(source) = source {
                candidates.push(Gpu { vendor, source });
            }
        }
        // Discrete GPUs (which actually have interesting sensors) sort first.
        candidates.sort_by_key(|g| match g.vendor {
            GpuVendor::Nvidia => 0,
            GpuVendor::Amd => 1,
            GpuVendor::Intel => 2,
        });
        candidates.into_iter().next()
    }

    /// Read the current temperature and power draw. Cheap for hwmon (two sysfs
    /// reads); an `nvidia-smi` invocation otherwise, so it stays off the render
    /// path like the CPU pollers.
    pub fn read(&self) -> GpuReading {
        match &self.source {
            GpuSource::Hwmon { temp, power } => GpuReading {
                temp: temp
                    .as_deref()
                    .and_then(read_i64)
                    .map(|m| (m / 1000) as i32),
                watts: power
                    .as_deref()
                    .and_then(read_i64)
                    .map(|u| (u as f64 / 1_000_000.0).round() as i32),
            },
            GpuSource::NvidiaSmi => read_nvidia_smi(),
        }
    }
}

/// Read a whole integer from a sysfs file (temperature in millidegrees, power in
/// microwatts).
fn read_i64(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Find the card's hwmon directory and the temp/power files within it. Returns
/// the paths only when at least one sensor is present.
fn find_hwmon(dev: &Path) -> Option<(Option<PathBuf>, Option<PathBuf>)> {
    for entry in fs::read_dir(dev.join("hwmon")).ok()?.flatten() {
        let hwmon = entry.path();
        let temp = Some(hwmon.join("temp1_input")).filter(|p| p.exists());
        let power = ["power1_average", "power1_input"]
            .into_iter()
            .map(|f| hwmon.join(f))
            .find(|p| p.exists());
        if temp.is_some() || power.is_some() {
            return Some((temp, power));
        }
    }
    None
}

/// Whether `nvidia-smi` is installed and runnable.
fn nvidia_smi_available() -> bool {
    Command::new("nvidia-smi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Query temperature and power from `nvidia-smi` in a single CSV line.
fn read_nvidia_smi() -> GpuReading {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return GpuReading::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // Only the first GPU is shown; multi-GPU rigs are out of scope.
    let mut fields = text.lines().next().unwrap_or("").split(',');
    let temp = fields
        .next()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v as i32);
    let watts = fields
        .next()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|v| v.round() as i32);
    GpuReading { temp, watts }
}
