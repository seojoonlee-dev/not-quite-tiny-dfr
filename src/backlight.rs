use crate::config::Config;
use anyhow::{anyhow, Result};
use input::event::{
    switch::{Switch, SwitchEvent, SwitchState},
    Event,
};
use std::{
    cmp::min,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

const MAX_DISPLAY_BRIGHTNESS: u32 = 509;
const MAX_TOUCH_BAR_BRIGHTNESS: u32 = 255;
const DIMMED_BRIGHTNESS: u32 = 1;

fn read_attr(path: &Path, attr: &str) -> u32 {
    fs::read_to_string(path.join(attr))
        .unwrap_or_else(|_| panic!("Failed to read {attr}"))
        .trim()
        .parse::<u32>()
        .unwrap_or_else(|_| panic!("Failed to parse {attr}"))
}

fn find_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if ["display-pipe", "228600000.dsi.0", "appletb_backlight"]
            .iter()
            .any(|s| name.contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Touch Bar backlight device found"))
}

fn find_display_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        if [
            "apple-panel-bl",
            "gmux_backlight",
            "intel_backlight",
            "acpi_video0",
        ]
        .iter()
        .any(|s| entry.file_name().to_string_lossy().contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Built-in Retina Display backlight device found"))
}

fn set_backlight(mut file: &File, value: u32) {
    file.write_all(format!("{}\n", value).as_bytes()).unwrap();
}

/// Best-effort: turn the Touch Bar backlight on. Used by the post-crash
/// emergency screen, where the normal backlight manager is long gone and the
/// bar may have been left dimmed or off.
pub fn force_on() {
    if let Ok(path) = find_backlight() {
        let _ = fs::write(path.join("brightness"), b"128\n");
    }
}

pub struct BacklightManager {
    last_active: Instant,
    max_bl: u32,
    current_bl: u32,
    lid_state: SwitchState,
    bl_file: File,
    display_bl_path: Option<PathBuf>,
}

impl BacklightManager {
    pub fn new() -> BacklightManager {
        let bl_path = find_backlight().unwrap();
        let display_bl_path = find_display_backlight()
            .inspect_err(|e| eprintln!("Failed to find display backlight sysfs path: {e}"))
            .ok();
        let bl_file = OpenOptions::new()
            .write(true)
            .open(bl_path.join("brightness"))
            .unwrap();
        BacklightManager {
            bl_file,
            lid_state: SwitchState::Off,
            max_bl: read_attr(&bl_path, "max_brightness"),
            current_bl: read_attr(&bl_path, "brightness"),
            last_active: Instant::now(),
            display_bl_path,
        }
    }
    fn display_to_touchbar(display: u32, active_brightness: u32) -> u32 {
        let normalized = display as f64 / MAX_DISPLAY_BRIGHTNESS as f64;
        // Add one so that the touch bar does not turn off
        let adjusted = (normalized.powf(0.5) * active_brightness as f64) as u32 + 1;
        adjusted.min(MAX_TOUCH_BAR_BRIGHTNESS) // Clamp the value to the maximum allowed brightness
    }
    pub fn process_event(&mut self, event: &Event) {
        match event {
            Event::Keyboard(_) | Event::Pointer(_) | Event::Gesture(_) | Event::Touch(_) => {
                self.last_active = Instant::now();
            }
            Event::Switch(SwitchEvent::Toggle(toggle)) => {
                if let Some(Switch::Lid) = toggle.switch() {
                    self.lid_state = toggle.switch_state();
                    println!("Lid Switch event: {:?}", self.lid_state);
                    if toggle.switch_state() == SwitchState::Off {
                        self.last_active = Instant::now();
                    }
                }
            }
            _ => {}
        }
    }
    pub fn update_backlight(&mut self, cfg: &Config) {
        let idle_ms = (Instant::now() - self.last_active).as_millis() as u64;
        // Timeouts are in seconds; 0 disables that transition entirely.
        let dim_ms = cfg.dim_timeout as u64 * 1000;
        let off_ms = cfg.off_timeout as u64 * 1000;

        let target = if self.lid_state == SwitchState::On {
            0
        } else if off_ms > 0 && idle_ms >= off_ms {
            0
        } else if dim_ms > 0 && idle_ms >= dim_ms {
            DIMMED_BRIGHTNESS
        } else if cfg.adaptive_brightness {
            let brightness = if let Some(path) = &self.display_bl_path {
                read_attr(path, "brightness")
            } else {
                self.max_bl / 2
            };
            BacklightManager::display_to_touchbar(brightness, cfg.active_brightness)
        } else {
            cfg.active_brightness
        };
        let new_bl = min(self.max_bl, target);
        if self.current_bl != new_bl {
            self.current_bl = new_bl;
            set_backlight(&self.bl_file, self.current_bl);
        }
    }
    pub fn current_bl(&self) -> u32 {
        self.current_bl
    }
}
