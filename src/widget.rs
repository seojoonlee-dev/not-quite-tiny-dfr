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

/// A command widget to run: unique id, the shell command, and how often to run.
#[derive(Clone)]
pub struct WidgetSpec {
    pub id: usize,
    pub command: String,
    pub interval: Duration,
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
}

impl WidgetRuntime {
    /// Spawn a worker thread per widget. `wake` is the write end of a pipe whose
    /// read end lives in the main epoll loop; a byte is written whenever a
    /// widget's output changes, so the loop wakes and redraws.
    pub fn new(specs: Vec<WidgetSpec>, wake: Arc<OwnedFd>) -> WidgetRuntime {
        let results: Arc<Mutex<HashMap<usize, WidgetOutput>>> = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        for spec in specs {
            let results = results.clone();
            let stop = stop.clone();
            let wake = wake.clone();
            thread::spawn(move || run_widget(spec, results, stop, wake));
        }
        WidgetRuntime { results, stop }
    }

    pub fn results(&self) -> MutexGuard<'_, HashMap<usize, WidgetOutput>> {
        self.results.lock().unwrap()
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
        assert_eq!(WidgetSpec::interval_from_secs(Some(5.0)), Duration::from_secs(5));
    }
}
