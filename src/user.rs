use nix::unistd::{Uid, User};
use std::{env, path::PathBuf, process::Command, thread::sleep, time::Duration};

/// How long to wait for seat0 to gain an active session before giving up and
/// falling back to `nobody`. The daemon is commonly started (via the Touch Bar
/// udev hotplug / graphical.target) a few seconds before the user's graphical
/// session finishes activating on seat0, so without this it would resolve no
/// user and never pick up ~/.config. Bounded so a genuinely user-less boot
/// (e.g. sitting at a greeter) still falls back in reasonable time.
const SEAT0_WAIT: Duration = Duration::from_secs(15);
const SEAT0_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A resolved target user whose per-user config (~/.config/not-quite-tiny-dfr) should be
/// read, and to whom privileges are dropped.
pub struct TargetUser {
    pub name: String,
    pub home: PathBuf,
}

/// Work out which user not-quite-tiny-dfr should serve. Resolution order:
///
/// 1. `NOT_QUITE_TINY_DFR_USER` env var — deterministic, and the recommended way when the
///    daemon may start before anyone has logged in (so logind has no session
///    yet). Set it from the systemd unit, e.g. `Environment=NOT_QUITE_TINY_DFR_USER=alice`.
/// 2. The user owning the active graphical session on seat0, via logind. Because
///    the daemon is usually started slightly before that session activates, we
///    poll logind for up to `SEAT0_WAIT` rather than checking just once.
///
/// Returns `None` when no user can be determined (e.g. started at boot with no
/// login within the wait window); the caller then falls back to `nobody` +
/// system config only.
pub fn resolve_target_user() -> Option<TargetUser> {
    // The env var is deterministic: honour it immediately and never wait on it.
    if let Ok(name) = env::var("NOT_QUITE_TINY_DFR_USER") {
        let name = name.trim();
        if !name.is_empty() {
            match from_name(name) {
                Some(u) => return Some(u),
                None => eprintln!("not-quite-tiny-dfr: NOT_QUITE_TINY_DFR_USER={name:?} not found, ignoring"),
            }
        }
    }
    wait_for_seat0_user()
}

/// Poll logind for seat0's active-session user, giving the graphical session a
/// bounded window to come up before falling back. Returns as soon as a user is
/// found (usually within a few seconds of a normal login).
fn wait_for_seat0_user() -> Option<TargetUser> {
    let deadline = std::time::Instant::now() + SEAT0_WAIT;
    loop {
        if let Some(u) = seat0_active_uid().and_then(from_uid) {
            return Some(u);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        sleep(SEAT0_POLL_INTERVAL);
    }
}

fn from_name(name: &str) -> Option<TargetUser> {
    let u = User::from_name(name).ok().flatten()?;
    Some(TargetUser {
        name: u.name,
        home: u.dir,
    })
}

fn from_uid(uid: u32) -> Option<TargetUser> {
    let u = User::from_uid(Uid::from_raw(uid)).ok().flatten()?;
    Some(TargetUser {
        name: u.name,
        home: u.dir,
    })
}

/// Query logind for the uid of the active session on seat0.
fn seat0_active_uid() -> Option<u32> {
    let session = loginctl_value(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
    if session.is_empty() {
        return None;
    }
    let uid = loginctl_value(&["show-session", &session, "-p", "User", "--value"])?;
    uid.parse().ok()
}

fn loginctl_value(args: &[&str]) -> Option<String> {
    let out = Command::new("loginctl").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
