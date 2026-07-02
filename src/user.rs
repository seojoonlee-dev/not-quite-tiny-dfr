use nix::unistd::{Uid, User};
use std::{env, path::PathBuf, process::Command};

/// A resolved target user whose per-user config (~/.config/not-quite-tiny-dfr) should be
/// read, and to whom privileges are dropped.
pub struct TargetUser {
    pub name: String,
    pub home: PathBuf,
}

/// Work out which user not-quite-tiny-dfr should serve, without blocking.
/// Resolution order:
///
/// 1. `NOT_QUITE_TINY_DFR_USER` env var — deterministic, and the recommended way
///    when the daemon may start before anyone has logged in.
/// 2. The user owning the active graphical session on seat0, via logind.
///
/// Returns `None` when no user is logged in yet (e.g. the daemon started at boot,
/// before the greeter). The caller then comes up on system config as root and
/// keeps calling this (see the main loop) until someone logs in — so a late
/// login still takes effect, rather than being locked out by an early fallback.
pub fn resolve_target_user() -> Option<TargetUser> {
    // The env var is deterministic: honour it immediately.
    if let Ok(name) = env::var("NOT_QUITE_TINY_DFR_USER") {
        let name = name.trim();
        if !name.is_empty() {
            match from_name(name) {
                Some(u) => return Some(u),
                None => eprintln!(
                    "not-quite-tiny-dfr: NOT_QUITE_TINY_DFR_USER={name:?} not found, ignoring"
                ),
            }
        }
    }
    seat0_active_uid().and_then(from_uid)
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

/// Query logind for the uid of the active *user* session on seat0.
///
/// We must check the session's `Class`: at boot the display manager runs a
/// greeter session that owns seat0's active session (e.g. SDDM as uid `sddm`)
/// before anyone logs in. Accepting it would make us drop privileges to the
/// greeter's account and load its (empty) config — and since privilege dropping
/// is one-way, we would then be locked out of ever serving the real user who
/// logs in afterwards. So we only accept `Class=user` sessions and keep polling
/// (as root) through the greeter until the actual login replaces it on seat0.
fn seat0_active_uid() -> Option<u32> {
    let session = loginctl_value(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
    if session.is_empty() {
        return None;
    }
    let class = loginctl_value(&["show-session", &session, "-p", "Class", "--value"])?;
    if class != "user" {
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
