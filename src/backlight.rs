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
    sync::atomic::{AtomicI32, Ordering},
    time::Instant,
};

const MAX_DISPLAY_BRIGHTNESS: u32 = 509;
const MAX_TOUCH_BAR_BRIGHTNESS: u32 = 255;
const DIMMED_BRIGHTNESS: u32 = 1;

static DIM_STEPS: AtomicI32 = AtomicI32::new(0);
// Direction of a currently held brightness button (+1/-1), 0 when released.
static DIM_HELD: AtomicI32 = AtomicI32::new(0);

// Hold-to-repeat: the press itself does the first step; held past the delay,
// further steps fire on an interval (each one glides, so a hold reads as one
// continuous ramp).
const DIM_REPEAT_DELAY: f64 = 0.4;
const DIM_REPEAT_INTERVAL: f64 = 0.2;

// The appletb hardware backlight only has full/dim/off, so manual brightness
// is done in software instead: the frame is multiplied by this factor at blit
// time. Levels are spaced geometrically (~0.81 ratio) so each press feels
// like an even step. The floor keeps the bar readable — at 0 the daemon
// ignores touches, so the buttons could never turn it back up.
const SOFT_DIM_FACTORS: [f64; 10] = [1.0, 0.81, 0.66, 0.53, 0.43, 0.35, 0.28, 0.23, 0.19, 0.15];
// Easing time constant between levels; a step settles in ~0.4-0.5s.
const SOFT_DIM_TAU: f64 = 0.12;

/// Press/release of a TouchBarBrightnessUp/Down button (+1/-1). A press queues one
/// step immediately; the hold state drives repeat in update_backlight.
pub fn dim_button(delta: i32, pressed: bool) {
    if pressed {
        DIM_STEPS.fetch_add(delta, Ordering::Relaxed);
        DIM_HELD.store(delta, Ordering::Relaxed);
    } else {
        DIM_HELD.store(0, Ordering::Relaxed);
    }
}

/// True while a brightness button is held; the main loop keeps its frame
/// timer armed so update_backlight gets called often enough to repeat.
pub fn dim_held() -> bool {
    DIM_HELD.load(Ordering::Relaxed) != 0
}

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
    // Index into SOFT_DIM_FACTORS, moved by the TouchBarBrightness buttons.
    // Not persisted across daemon restarts.
    soft_dim_step: usize,
    // Animated blit factor easing toward SOFT_DIM_FACTORS[soft_dim_step].
    soft_dim_current: f64,
    soft_dim_anim_ts: Instant,
    // Hold-to-repeat bookkeeping for the brightness buttons.
    dim_hold_dir: i32,
    dim_hold_since: Instant,
    dim_last_repeat: Instant,
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
            soft_dim_step: 0,
            soft_dim_current: 1.0,
            soft_dim_anim_ts: Instant::now(),
            dim_hold_dir: 0,
            dim_hold_since: Instant::now(),
            dim_last_repeat: Instant::now(),
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

        let anim_now = Instant::now();

        let held = DIM_HELD.load(Ordering::Relaxed);
        if held != self.dim_hold_dir {
            self.dim_hold_dir = held;
            self.dim_hold_since = anim_now;
            self.dim_last_repeat = anim_now;
        } else if held != 0
            && (anim_now - self.dim_hold_since).as_secs_f64() >= DIM_REPEAT_DELAY
            && (anim_now - self.dim_last_repeat).as_secs_f64() >= DIM_REPEAT_INTERVAL
        {
            self.dim_last_repeat = anim_now;
            DIM_STEPS.fetch_add(held, Ordering::Relaxed);
        }

        let steps = DIM_STEPS.swap(0, Ordering::Relaxed);
        if steps != 0 {
            // Up (+1) means brighter = lower index.
            self.soft_dim_step = (self.soft_dim_step as i32 - steps)
                .clamp(0, SOFT_DIM_FACTORS.len() as i32 - 1)
                as usize;
        }
        // Ease the blit factor toward the selected level. dt is capped so the
        // first pass after an idle stretch counts as one tick, not a jump to
        // the target.
        let dt = (anim_now - self.soft_dim_anim_ts).as_secs_f64().min(0.05);
        self.soft_dim_anim_ts = anim_now;
        let dim_target = SOFT_DIM_FACTORS[self.soft_dim_step];
        let diff = dim_target - self.soft_dim_current;
        self.soft_dim_current = if diff.abs() < 0.003 {
            dim_target
        } else {
            self.soft_dim_current + diff * (1.0 - (-dt / SOFT_DIM_TAU).exp())
        };

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
    pub fn soft_dim_factor(&self) -> f64 {
        self.soft_dim_current
    }
    pub fn soft_dim_animating(&self) -> bool {
        self.soft_dim_current != SOFT_DIM_FACTORS[self.soft_dim_step]
    }
}
