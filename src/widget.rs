use crate::style::Color;
use libc::c_void;
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::Read,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

/// Hard cap so a hung script can't wedge its worker thread forever.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Floor on the poll interval so nobody cooks their CPU with `Interval = 0.01`.
const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// How often worker threads wake to re-check the stop flag while waiting.
const POLL_STEP: Duration = Duration::from_millis(50);
/// How long after a slider set to distrust polled get results: a poll that
/// STARTED before the set can finish after it carrying the pre-set value,
/// and applying it would snap the slider's fill backwards.
const SET_QUIET: Duration = Duration::from_millis(1500);

/// A command widget to run: unique id, the shell command, and how often to run.
#[derive(Clone)]
pub struct WidgetSpec {
    pub id: usize,
    pub command: String,
    pub interval: Duration,
}

/// A slider widget: `get_command` prints the current value (0-100, optionally
/// followed by the word "muted") and is polled like a command widget;
/// `set_command` is run with `{}` replaced by the new value whenever the user
/// moves the slider; `mute_command` (optional) is run with `{}` replaced by
/// "toggle" (icon tap) or "0" (a drag unmutes).
#[derive(Clone)]
pub struct SliderSpec {
    pub id: usize,
    pub get_command: String,
    pub set_command: String,
    pub mute_command: Option<String>,
    pub interval: Duration,
}

/// Substitute `arg` into a command template: replaces `{}` when present,
/// otherwise appends it as an argument.
fn fill_placeholder(command: &str, arg: &str) -> String {
    if command.contains("{}") {
        command.replace("{}", arg)
    } else {
        format!("{command} {arg}")
    }
}

/// All the command-running widgets a config produced.
#[derive(Default)]
pub struct Widgets {
    pub commands: Vec<WidgetSpec>,
    pub sliders: Vec<SliderSpec>,
}

impl WidgetSpec {
    /// Turn a configured `Interval` (seconds) into a clamped `Duration`.
    pub fn interval_from_secs(secs: Option<f64>) -> Duration {
        let secs = secs.unwrap_or(2.0);
        if secs.is_finite() && secs > 0.0 {
            Duration::from_secs_f64(secs).max(MIN_INTERVAL)
        } else {
            MIN_INTERVAL
        }
    }
}

/// The latest output of a widget script.
#[derive(Clone, Default, PartialEq)]
pub struct WidgetOutput {
    pub text: String,
    pub color: Option<Color>,
}

/// The optional JSON form a script may print for richer control.
#[derive(Deserialize)]
struct WidgetJson {
    text: Option<String>,
    color: Option<String>,
}

/// Owns the worker threads that poll widget commands. Dropping it signals the
/// threads to stop; they are detached (not joined) so a config reload stays
/// snappy even if a script is mid-run -- the old threads notice the flag and
/// exit on their own.
pub struct WidgetRuntime {
    results: Arc<Mutex<HashMap<usize, WidgetOutput>>>,
    stop: Arc<AtomicBool>,
    /// Latest slider value awaiting its set command, per slider id. The setter
    /// thread drains this; a fast drag coalesces to the newest value instead
    /// of queueing a process run per pixel.
    pending_sets: Arc<Mutex<HashMap<usize, i32>>>,
    set_commands: HashMap<usize, String>,
    /// Fully-substituted mute commands awaiting the setter thread.
    pending_mutes: Arc<Mutex<HashMap<usize, String>>>,
    mute_commands: HashMap<usize, String>,
    /// When each slider was last set or (un)muted, for the SET_QUIET window.
    last_set: Arc<Mutex<HashMap<usize, Instant>>>,
}

impl WidgetRuntime {
    /// Spawn a worker thread per widget. `wake` is the write end of a pipe whose
    /// read end lives in the main epoll loop; a byte is written whenever a
    /// widget's output changes, so the loop wakes and redraws.
    pub fn new(widgets: Widgets, wake: Arc<OwnedFd>) -> WidgetRuntime {
        let results: Arc<Mutex<HashMap<usize, WidgetOutput>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        for spec in widgets.commands {
            let results = results.clone();
            let stop = stop.clone();
            let wake = wake.clone();
            thread::spawn(move || run_widget(spec, results, stop, wake));
        }
        // Sliders poll their get command through the same worker path.
        let mut set_commands = HashMap::new();
        for spec in &widgets.sliders {
            set_commands.insert(spec.id, spec.set_command.clone());
            let poll = WidgetSpec {
                id: spec.id,
                command: spec.get_command.clone(),
                interval: spec.interval,
            };
            let results = results.clone();
            let stop = stop.clone();
            let wake = wake.clone();
            thread::spawn(move || run_widget(poll, results, stop, wake));
        }
        let pending_sets: Arc<Mutex<HashMap<usize, i32>>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_mutes: Arc<Mutex<HashMap<usize, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mute_commands: HashMap<usize, String> = widgets
            .sliders
            .iter()
            .filter_map(|s| Some((s.id, s.mute_command.clone()?)))
            .collect();
        if !widgets.sliders.is_empty() {
            let pending = pending_sets.clone();
            let mutes = pending_mutes.clone();
            let stop_flag = stop.clone();
            let commands = set_commands.clone();
            let get_commands: HashMap<usize, String> = widgets
                .sliders
                .iter()
                .map(|s| (s.id, s.get_command.clone()))
                .collect();
            let setter_results = results.clone();
            thread::spawn(move || {
                run_setter(pending, mutes, commands, get_commands, setter_results, stop_flag)
            });
        }
        WidgetRuntime {
            results,
            stop,
            pending_sets,
            set_commands,
            pending_mutes,
            mute_commands,
            last_set: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn results(&self) -> MutexGuard<'_, HashMap<usize, WidgetOutput>> {
        self.results.lock().unwrap()
    }

    /// Queue a slider's set command with the new value. Coalesces: only the
    /// latest value per slider is kept, and the setter thread runs the actual
    /// command off the render path.
    pub fn set_slider(&self, id: usize, value: i32) {
        if !self.set_commands.contains_key(&id) {
            return;
        }
        self.last_set.lock().unwrap().insert(id, Instant::now());
        // Reflect the value in the results cache right away, so a post-drag
        // apply can't snap the fill back to a pre-drag poll reading.
        self.results.lock().unwrap().insert(
            id,
            WidgetOutput {
                text: value.to_string(),
                color: None,
            },
        );
        self.pending_sets.lock().unwrap().insert(id, value);
    }

    /// Queue a slider's mute command. `arg` is "toggle" (icon tap) or "0"
    /// (deterministic unmute when a drag moves the volume).
    pub fn set_slider_mute(&self, id: usize, arg: &str) {
        if let Some(command) = self.mute_commands.get(&id) {
            self.last_set.lock().unwrap().insert(id, Instant::now());
            self.pending_mutes
                .lock()
                .unwrap()
                .insert(id, fill_placeholder(command, arg));
        }
    }

    /// Whether `id` was set recently enough that polled get results may still
    /// predate the set and should not be applied.
    pub fn recently_set(&self, id: usize) -> bool {
        self.last_set
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|t| t.elapsed() < SET_QUIET)
    }
}

/// Drain queued slider set commands, running each to completion so no zombie
/// processes accumulate. One thread serves all sliders: set commands are
/// near-instant (wpctl, brightnessctl), and coalescing means a laggy one only
/// skips intermediate values, never queues them up.
fn run_setter(
    pending: Arc<Mutex<HashMap<usize, i32>>>,
    pending_mutes: Arc<Mutex<HashMap<usize, String>>>,
    commands: HashMap<usize, String>,
    get_commands: HashMap<usize, String>,
    results: Arc<Mutex<HashMap<usize, WidgetOutput>>>,
    stop: Arc<AtomicBool>,
) {
    // Refresh the cache from the get command once a set/mute has been
    // applied, so the poller's next read agrees -- unless the drag already
    // queued a newer value, in which case that one is about to run anyway.
    let refresh = |id: usize, stop: &AtomicBool| {
        if let Some(get) = get_commands.get(&id) {
            let out = run_command(get, stop);
            if !pending.lock().unwrap().contains_key(&id)
                && !pending_mutes.lock().unwrap().contains_key(&id)
            {
                results.lock().unwrap().insert(id, out);
            }
        }
    };
    let pop_any = |map: &Mutex<HashMap<usize, String>>| {
        let mut map = map.lock().unwrap();
        map.keys().next().copied().map(|id| {
            let cmd = map.remove(&id).unwrap();
            (id, cmd)
        })
    };
    while !stop.load(Ordering::Relaxed) {
        // Mutes drain first: an unmute queued together with a drag's first
        // value must apply before the volume does.
        if let Some((id, command)) = pop_any(&pending_mutes) {
            let _ = run_command(&command, &stop);
            refresh(id, &stop);
            continue;
        }
        let next = {
            let mut map = pending.lock().unwrap();
            map.keys().next().copied().map(|id| {
                let value = map.remove(&id).unwrap();
                (id, value)
            })
        };
        match next {
            Some((id, value)) => {
                if let Some(command) = commands.get(&id) {
                    // Reuses the widget command runner for its timeout and
                    // stop handling; the output is irrelevant.
                    let _ = run_command(&fill_placeholder(command, &value.to_string()), &stop);
                }
                refresh(id, &stop);
            }
            None => thread::sleep(POLL_STEP),
        }
    }
}

impl Drop for WidgetRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_widget(
    spec: WidgetSpec,
    results: Arc<Mutex<HashMap<usize, WidgetOutput>>>,
    stop: Arc<AtomicBool>,
    wake: Arc<OwnedFd>,
) {
    while !stop.load(Ordering::Relaxed) {
        let output = run_command(&spec.command, &stop);
        let changed = {
            let mut map = results.lock().unwrap();
            if map.get(&spec.id) != Some(&output) {
                map.insert(spec.id, output);
                true
            } else {
                false
            }
        };
        if changed {
            // Wake the main loop. One byte is enough; it drains all of them.
            let byte = [1u8];
            unsafe {
                libc::write(wake.as_raw_fd(), byte.as_ptr() as *const c_void, 1);
            }
        }
        sleep_until(Instant::now() + spec.interval, &stop);
    }
}

/// Sleep until `deadline`, waking early (and returning) if `stop` is set.
fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(POLL_STEP.min(deadline - now));
    }
}

/// Run `sh -c <command>`, returning its parsed output. Kills the child if it
/// exceeds the timeout or the runtime is stopping.
fn run_command(command: &str, stop: &AtomicBool) -> WidgetOutput {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return WidgetOutput {
                text: format!("err: {e}"),
                color: None,
            }
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let timed_out = start.elapsed() > COMMAND_TIMEOUT;
                if timed_out || stop.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return if timed_out {
                        WidgetOutput {
                            text: "timeout".into(),
                            color: None,
                        }
                    } else {
                        WidgetOutput::default()
                    };
                }
                thread::sleep(POLL_STEP);
            }
            Err(_) => break,
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    parse_output(&out)
}

/// Parse a widget's stdout: JSON `{"text","color"}` if it looks like JSON,
/// otherwise the first non-empty line as plain text.
fn parse_output(raw: &str) -> WidgetOutput {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return WidgetOutput::default();
    }
    if trimmed.starts_with('{') {
        if let Ok(j) = serde_json::from_str::<WidgetJson>(trimmed) {
            return WidgetOutput {
                text: j.text.unwrap_or_default(),
                color: j.color.as_deref().and_then(Color::parse_hex),
            };
        }
        // Looked like JSON but wasn't valid; fall through to plain text.
    }
    WidgetOutput {
        text: trimmed.lines().next().unwrap_or_default().to_string(),
        color: None,
    }
}

/// Set `O_NONBLOCK` on a fd (the wake pipe's read end, so draining never blocks).
pub fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Drain any pending wake bytes from the pipe read end (non-blocking).
pub fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text_first_line() {
        let o = parse_output("42%\nignored\n");
        assert_eq!(o.text, "42%");
        assert_eq!(o.color, None);
    }

    #[test]
    fn parses_json_text_and_color() {
        let o = parse_output(r##"{"text":"42%","color":"#ff0000"}"##);
        assert_eq!(o.text, "42%");
        assert_eq!(o.color, Some(Color::rgb(1.0, 0.0, 0.0)));
    }

    #[test]
    fn invalid_json_falls_back_to_text() {
        assert_eq!(parse_output("{not json").text, "{not json");
        assert_eq!(parse_output("   ").text, ""); // whitespace -> empty
    }

    #[test]
    fn interval_is_clamped_to_floor_and_defaulted() {
        assert_eq!(WidgetSpec::interval_from_secs(Some(0.001)), MIN_INTERVAL);
        assert_eq!(WidgetSpec::interval_from_secs(Some(-5.0)), MIN_INTERVAL);
        assert_eq!(WidgetSpec::interval_from_secs(None), Duration::from_secs(2));
        assert_eq!(
            WidgetSpec::interval_from_secs(Some(5.0)),
            Duration::from_secs(5)
        );
    }
}
