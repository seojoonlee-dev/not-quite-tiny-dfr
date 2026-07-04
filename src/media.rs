//! Now-playing state for the Media widget. A poller thread runs `playerctl`
//! (MPRIS) off the render path -- mirroring the battery/CPU pollers -- and
//! publishes the current status, track text, and decoded album art into
//! `MEDIA_STATE`, waking the event loop through the shared pipe on any change.
//!
//! Album art is decoded to a cairo-ready ARGB32 pixel buffer here (the `image`
//! crate handles JPEG/PNG, which cairo cannot load itself) and stored as raw
//! bytes: cairo `ImageSurface`s are not `Send`, so the render thread wraps the
//! bytes into a surface itself (see `main.rs`).

use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cairo::Format;

/// How often the poller re-queries playerctl.
const POLL: Duration = Duration::from_millis(700);
/// Album art is downscaled so its longest side is at most this many pixels:
/// plenty for the short bar panel while keeping the buffer small.
const ART_MAX: u32 = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MediaStatus {
    /// No player, or a stopped one: the widget shows its idle transport row.
    Idle,
    Playing,
    Paused,
}

/// Album art decoded into a cairo ARGB32 (premultiplied, native-endian) buffer.
pub struct MediaArt {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub data: Vec<u8>,
}

pub struct MediaInfo {
    pub status: MediaStatus,
    pub title: String,
    pub artist: String,
    pub art: Option<MediaArt>,
    /// Bumped on every published change; the render loop redraws the Media
    /// button only when this moves (like the battery/CPU cached readings).
    pub generation: u64,
}

impl MediaInfo {
    /// The now-playing panel shows for a real track, playing or paused (so the
    /// cover stays up while paused, and through a track's paused transitions).
    /// Only Idle collapses to the transport row -- and YouTube ads are forced
    /// to Idle by the poller so they still stay hidden.
    pub fn is_active(&self) -> bool {
        self.status != MediaStatus::Idle
    }
}

pub static MEDIA_STATE: Mutex<MediaInfo> = Mutex::new(MediaInfo {
    status: MediaStatus::Idle,
    title: String::new(),
    artist: String::new(),
    art: None,
    generation: 0,
});

/// Run a `playerctl` control verb (`play-pause`, `next`, `previous`) for the
/// active player, ignoring failure (there may be no player).
pub fn control(verb: &str) {
    let _ = Command::new("playerctl").arg(verb).status();
}

/// Spawn the poller thread. `wake` is the write end of the loop's wake pipe.
pub fn spawn_poller(wake: Arc<OwnedFd>) {
    thread::spawn(move || {
        // The art URL currently decoded into MEDIA_STATE, so unchanged art is
        // never re-decoded (and a failing URL is not retried every tick).
        let mut cur_art_url = String::new();
        loop {
            let (mut status, title, artist, art_url, page_url) = query();
            // YouTube ad guard. During an ad YouTube swaps the session's URL to
            // plain `youtube.com` (no `watch?v=<id>`) while still reporting
            // "Playing" with the ad's title -- and sometimes its own art -- so
            // the panel would pop up showing the ad, including after you leave a
            // video. A real video always carries a video id, so a YouTube-host
            // URL with none is an ad (or the homepage autoplaying): report it
            // inactive to keep the panel collapsed. (Verified against live MPRIS
            // output.)
            let is_youtube_ad = is_ad_url(&page_url);
            if is_youtube_ad {
                // Idle (not Paused) so the panel collapses -- Paused now stays
                // visible for real tracks.
                status = MediaStatus::Idle;
            }
            // The art source: a stable YouTube thumbnail derived from the page
            // URL when applicable, else the player's own art URL. Skipped for an
            // ad so its art never replaces the real cover. Only (re)load for a
            // new, non-empty source -- browsers flap `mpris:artUrl` to "" mid-
            // track, and clearing on those blanks flickered the cover to black,
            // so an empty source is ignored and the current art kept.
            let art_source = if is_youtube_ad {
                String::new()
            } else {
                art_source_url(&art_url, &page_url)
            };
            let art_reload = !art_source.is_empty() && art_source != cur_art_url;
            // Decode outside the lock: fetch/decode must not stall render reads.
            let decoded = if art_reload {
                load_art(&art_source, status)
            } else {
                None
            };
            if art_reload && std::env::var_os("NQTD_MEDIA_LOG").is_some() {
                eprintln!(
                    "nqtd media: art_source={art_source:?} loaded={}",
                    decoded.is_some()
                );
            }
            {
                let mut shared = MEDIA_STATE.lock().unwrap();
                let meta_changed = shared.status != status
                    || shared.title != title
                    || shared.artist != artist;
                if art_reload {
                    shared.art = decoded;
                }
                if meta_changed || art_reload {
                    shared.status = status;
                    shared.title = title;
                    shared.artist = artist;
                    shared.generation = shared.generation.wrapping_add(1);
                    let byte = [1u8];
                    unsafe {
                        libc::write(wake.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
                    }
                }
            }
            if art_reload {
                cur_art_url = art_source;
            }
            thread::sleep(POLL);
        }
    });
}

/// Status, title, artist, art URL, and page URL (`xesam:url`) for one player.
type Track = (MediaStatus, String, String, String, String);

const PLAYERCTL_FORMAT: &str =
    "{{status}}\u{1f}{{xesam:title}}\u{1f}{{xesam:artist}}\u{1f}{{mpris:artUrl}}\u{1f}{{xesam:url}}";

/// Which player to follow. The most-recently-active player (`playerctld`) is
/// preferred, but a paused most-recent player yields to one that is actually
/// playing -- so starting playback in another app switches to it. Empty strings
/// when there is no player.
fn query() -> Track {
    let latest = run_query(&["-p", "playerctld"]);
    // Most-recent player is itself playing a real track: follow it.
    let latest_playing = latest
        .as_ref()
        .is_some_and(|t| t.0 == MediaStatus::Playing && !is_ad_url(&t.4));
    if latest_playing {
        return latest.unwrap();
    }
    // It's paused/idle (or on an ad): switch to whatever is actually playing.
    if let Some(playing) = first_playing() {
        return playing;
    }
    // Nothing playing: show the most-recent (e.g. a paused track), else the
    // default pick, else idle.
    latest.or_else(|| run_query(&[])).unwrap_or((
        MediaStatus::Idle,
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ))
}

/// A YouTube ad: a YouTube-host URL with no video id (real videos always carry
/// one; YouTube swaps the URL to plain youtube.com during ads).
fn is_ad_url(url: &str) -> bool {
    is_youtube_host(url) && youtube_id(url).is_none()
}

/// Parse one playerctl format line (see `PLAYERCTL_FORMAT`).
fn parse_line(line: &str) -> Track {
    let mut parts = line.splitn(5, '\u{1f}');
    let status = match parts.next().unwrap_or("") {
        "Playing" => MediaStatus::Playing,
        "Paused" => MediaStatus::Paused,
        _ => MediaStatus::Idle,
    };
    let title = parts.next().unwrap_or("").to_string();
    let artist = parts.next().unwrap_or("").to_string();
    let art_url = parts.next().unwrap_or("").to_string();
    let page_url = parts.next().unwrap_or("").to_string();
    (status, title, artist, art_url, page_url)
}

/// Query a specific player selection (`player_args`, e.g. `-p playerctld`).
/// `None` when the command fails or no player is present.
fn run_query(player_args: &[&str]) -> Option<Track> {
    let out = Command::new("playerctl")
        .args(player_args)
        .args(["metadata", "--format", PLAYERCTL_FORMAT])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().filter(|l| !l.is_empty())?;
    Some(parse_line(line))
}

/// Scan all players for one that is playing, preferring a real track over a
/// YouTube ad; `None` if nothing is playing.
fn first_playing() -> Option<Track> {
    let out = Command::new("playerctl")
        .args(["-a", "metadata", "--format", PLAYERCTL_FORMAT])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut best: Option<(i32, Track)> = None;
    for line in text.lines() {
        let track = parse_line(line);
        if track.0 != MediaStatus::Playing {
            continue;
        }
        let rank = if is_ad_url(&track.4) { 1 } else { 2 };
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, track));
        }
    }
    best.map(|(_, t)| t)
}

/// The URL to load album art from: a YouTube thumbnail derived from the page
/// URL when applicable (browsers rarely expose a usable `mpris:artUrl`, but
/// `xesam:url` is stable), otherwise the player's own `mpris:artUrl`.
fn art_source_url(art_url: &str, page_url: &str) -> String {
    youtube_thumb(page_url).unwrap_or_else(|| art_url.to_string())
}

/// Whether a page URL is on a YouTube host.
fn is_youtube_host(page_url: &str) -> bool {
    page_url.contains("youtube.com") || page_url.contains("youtu.be")
}

/// The 11-char video id of a YouTube watch/shorts/youtu.be page, or `None`.
fn youtube_id(page_url: &str) -> Option<String> {
    if !is_youtube_host(page_url) {
        return None;
    }
    let take_id = |s: &str| -> Option<String> {
        let id: String = s
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (id.len() == 11).then_some(id)
    };
    ["v=", "youtu.be/", "/shorts/"]
        .iter()
        .find_map(|marker| page_url.find(marker).and_then(|i| take_id(&page_url[i + marker.len()..])))
}

/// The thumbnail URL for a YouTube page, or `None`.
fn youtube_thumb(page_url: &str) -> Option<String> {
    youtube_id(page_url).map(|id| format!("https://img.youtube.com/vi/{id}/hqdefault.jpg"))
}

/// Decode an art URL into a cairo ARGB32 buffer, downscaled to `ART_MAX`.
/// Handles `file://` paths and `http(s)://` URLs (e.g. browser/YouTube
/// thumbnails, fetched with curl). Returns `None` for idle players, an empty
/// or unsupported URL, or any fetch/decode failure -- the widget then draws a
/// plain panel.
fn load_art(url: &str, status: MediaStatus) -> Option<MediaArt> {
    if status == MediaStatus::Idle || url.is_empty() {
        return None;
    }
    let bytes = if let Some(path) = url.strip_prefix("file://") {
        std::fs::read(percent_decode(path)).ok()?
    } else if url.starts_with("http://") || url.starts_with("https://") {
        fetch_url(url)?
    } else {
        return None;
    };
    let img = image::load_from_memory(&bytes).ok()?.thumbnail(ART_MAX, ART_MAX);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    if w == 0 || h == 0 {
        return None;
    }
    let stride = Format::ARgb32.stride_for_width(w as u32).ok()?;
    let mut data = vec![0u8; (stride * h) as usize];
    for (x, y, px) in rgba.enumerate_pixels() {
        let [r, g, b, a] = px.0;
        let a32 = a as u32;
        // cairo ARGB32 is premultiplied, laid out BGRA on little-endian.
        let off = (y as i32 * stride + x as i32 * 4) as usize;
        data[off] = (b as u32 * a32 / 255) as u8;
        data[off + 1] = (g as u32 * a32 / 255) as u8;
        data[off + 2] = (r as u32 * a32 / 255) as u8;
        data[off + 3] = a;
    }
    Some(MediaArt {
        width: w,
        height: h,
        stride,
        data,
    })
}

/// Fetch an http(s) art URL into memory with curl, bounded in time and size so
/// a slow or huge response can't stall the poller or balloon memory.
fn fetch_url(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-sL", "--max-time", "5", "--max-filesize", "8M", url])
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// Minimal percent-decoding for `file://` paths (spaces etc. arrive as `%20`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
