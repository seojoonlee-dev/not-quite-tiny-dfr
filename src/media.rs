//! Now-playing state for the Media widget. A poller thread runs `playerctl`
//! (MPRIS) off the render path -- mirroring the battery/CPU pollers -- and
//! publishes the current status, track text, and decoded album art into
//! `MEDIA_STATE`, waking the event loop through the shared pipe on any change.
//!
//! Album art is decoded to a cairo-ready ARGB32 pixel buffer here (the `image`
//! crate handles JPEG/PNG, which cairo cannot load itself) and stored as raw
//! bytes: cairo `ImageSurface`s are not `Send`, so the render thread wraps the
//! bytes into a surface itself (see `main.rs`).

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use cairo::Format;
use serde_json::Value;

/// How often the poller re-queries playerctl metadata.
const POLL: Duration = Duration::from_millis(700);
/// How often the poller re-derives the current lyric line (cheap arithmetic
/// off the metadata cadence, so the highlighted line tracks the song closely).
const LYRIC_TICK: Duration = Duration::from_millis(100);
/// How long a lyric gap must last (in playback seconds) before the widget
/// switches from the lyrics to the controls/title. Short gaps between lines
/// would otherwise flap the view; only a real break (intro / instrumental)
/// should surface the controls.
const GAP_DEBOUNCE: f64 = 1.0;
/// A backward position jump larger than this (seconds) is treated as a real
/// seek; anything smaller is playerctl re-anchor jitter and is ignored, so the
/// highlighted lyric never bounces back a line mid-transition.
const SEEK_BACK_THRESHOLD: f64 = 2.0;
/// Album art is downscaled so its longest side is at most this many pixels:
/// plenty for the short bar panel while keeping the buffer small.
const ART_MAX: u32 = 256;
/// Backoff for an art URL whose fetch/decode failed: first retry after
/// `ART_RETRY_MIN`, doubling up to `ART_RETRY_MAX`, for as long as the URL
/// stays current -- so a transient miss (curl timeout, CDN 404 while the image
/// propagates) recovers instead of losing the cover for the whole track.
const ART_RETRY_MIN: Duration = Duration::from_secs(2);
const ART_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaStatus {
    /// No player, or a stopped one: the widget shows its idle transport row.
    #[default]
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

/// Run the media widget's OnClick shell command, fire-and-forget. A thread
/// waits on the child (reaping it) so an arbitrary command can't stall the
/// render loop.
pub fn run_tap_command(command: &str) {
    let command = command.to_string();
    thread::spawn(move || {
        if let Err(e) = Command::new("sh").arg("-c").arg(&command).status() {
            eprintln!("not-quite-tiny-dfr: failed to run OnClick command {command:?}: {e}");
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum LyricsStatus {
    /// No lyrics (nothing playing, or none found for the track).
    #[default]
    None,
    /// A fetch is in flight for the current track.
    Loading,
    /// Timed lyrics are available.
    Synced,
}

/// Timed lyrics for the current track, fetched from lrclib in the background.
pub struct LyricsInfo {
    /// The `title\u{1f}artist` these lyrics belong to (guards against a late
    /// fetch landing after the track already changed).
    pub track_key: String,
    pub status: LyricsStatus,
    /// `(start_seconds, text)`, sorted by time.
    pub lines: Vec<(f64, String)>,
    /// Index of the line active at the current playback position.
    pub current: usize,
    /// Whether the playback position currently sits in a lyric gap -- before the
    /// first line, or on a blank line (instrumental breaks / bridges are marked
    /// with empty lines in LRC). The widget shows the controls/title then.
    pub in_gap: bool,
    /// Bumped whenever the display should change (lines loaded, line advanced).
    pub generation: u64,
}

impl LyricsInfo {
    pub fn has_lyrics(&self) -> bool {
        self.status == LyricsStatus::Synced && !self.lines.is_empty()
    }
}

pub static LYRICS_STATE: Mutex<LyricsInfo> = Mutex::new(LyricsInfo {
    track_key: String::new(),
    status: LyricsStatus::None,
    lines: Vec::new(),
    current: 0,
    in_gap: false,
    generation: 0,
});

/// Wake the event loop through the shared pipe.
fn notify(wake: &OwnedFd) {
    let byte = [1u8];
    unsafe {
        libc::write(wake.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
    }
}

/// Index of the lyric line active at `time` seconds: the last line whose start
/// is at or before `time` (with a small fudge so it flips slightly early), or
/// the first line before the lyrics begin. `None` when there are no lines.
fn index_for_time(lines: &[(f64, String)], time: f64) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    let target = time + 0.2;
    // Binary search for the number of lines starting at or before `target`.
    let mut lo = 0usize;
    let mut hi = lines.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if lines[mid].0 <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(lo.saturating_sub(1))
}

/// Whether `time` sits in a lyric gap: before the first line begins, or on a
/// line whose text is blank. LRC marks instrumental breaks / bridges with empty
/// lines, so this catches the intro and mid-song gaps the vocals drop out for.
fn gap_at(lines: &[(f64, String)], time: f64) -> bool {
    let Some(first) = lines.first() else {
        return true;
    };
    if time + 0.2 < first.0 {
        return true; // still in the intro, before any lyric
    }
    match index_for_time(lines, time) {
        Some(i) => lines.get(i).map_or(true, |l| l.1.trim().is_empty()),
        None => true,
    }
}

/// Parse LRC synced lyrics into `(start_seconds, text)` pairs, sorted by time.
/// A line may carry several timestamps (`[t1][t2]text`); each yields an entry.
fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out: Vec<(f64, String)> = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        let mut times = Vec::new();
        // Consume leading `[..]` tags; keep the numeric (time) ones.
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(close) = after_open.find(']') else {
                break;
            };
            let tag = &after_open[..close];
            match parse_lrc_time(tag) {
                Some(t) => {
                    times.push(t);
                    rest = &after_open[close + 1..];
                }
                None => break, // a metadata tag like [ar:...]; not a lyric line
            }
        }
        if times.is_empty() {
            continue;
        }
        let lyric = rest.trim().to_string();
        // Drop leading credit/metadata lines (NetEase prefixes its LRC with
        // "作词 : X" / "Lyricist: X" and the like) -- shown as lyrics they read
        // as noise.
        let first = times.iter().cloned().fold(f64::INFINITY, f64::min);
        if first < 20.0 && is_credit_line(&lyric) {
            continue;
        }
        for t in times {
            out.push((t, lyric.clone()));
        }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// A leading credit/metadata line (composer, lyricist, etc.) rather than an
/// actual sung lyric. NetEase prefixes its LRC with these, and they read as
/// noise if surfaced as lyrics.
fn is_credit_line(s: &str) -> bool {
    const KEYWORDS: [&str; 14] = [
        "作词", "作曲", "编曲", "制作", "收录", "演奏", "词：", "曲：", "Lyricist",
        "Composer", "Arranger", "Producer", "Mixing", "Mastering",
    ];
    let lower = s.to_lowercase();
    let has_kw = KEYWORDS
        .iter()
        // ASCII keywords match case-insensitively; CJK ones as-is.
        .any(|k| if k.is_ascii() { lower.contains(&k.to_lowercase()) } else { s.contains(k) });
    has_kw && (s.contains(':') || s.contains('：') || s.chars().count() < 25)
}

/// Parse an LRC timestamp `mm:ss(.xx)` into seconds; `None` for a non-time tag.
fn parse_lrc_time(tag: &str) -> Option<f64> {
    let (m, s) = tag.split_once(':')?;
    let minutes: f64 = m.trim().parse().ok()?;
    let seconds: f64 = s.trim().parse().ok()?;
    Some(minutes * 60.0 + seconds)
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept as-is).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// GET a URL as text via curl (bounded time), or `None` on failure.
fn curl_get(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-sfL",
            "--compressed",       // gzip the response (smaller transfer)
            "--connect-timeout",  // fail a dead connection fast
            "5",
            "--max-time",
            "10",
            "-A",
            "not-quite-tiny-dfr (https://github.com/seojoonlee-dev/not-quite-tiny-dfr)",
            url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A non-empty `syncedLyrics` string from an lrclib object, if present.
fn synced_from_object(v: &Value) -> Option<String> {
    let s = v.get("syncedLyrics")?.as_str()?;
    (!s.is_empty()).then(|| s.to_string())
}

/// GET a NetEase (music.163.com) endpoint as text. NetEase rejects the default
/// User-Agent, so it needs a browser UA and a matching Referer.
fn netease_get(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-sfL",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "-A",
            "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0",
            "-e",
            "https://music.163.com/",
            url,
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fetch synced lyrics from NetEase, the fallback for tracks lrclib doesn't
/// cover (its catalog is large, especially for non-English music). Searches by
/// "title artist", then tries each song whose artist matches ours
/// case-insensitively (either direction, mirroring the lrclib guard) and
/// returns the first non-empty LRC -- catalog entries with no lyrics are
/// common, so committing to a single candidate loses tracks the next
/// candidate covers.
fn fetch_netease(title: &str, artist: &str) -> Option<String> {
    let query = urlencode(&format!("{title} {artist}"));
    let search = format!("https://music.163.com/api/search/get?s={query}&type=1&limit=5");
    let results: Value = serde_json::from_str(&netease_get(&search)?).ok()?;
    let songs = results.get("result")?.get("songs")?.as_array()?;

    let artist_lc = artist.to_lowercase();
    songs
        .iter()
        .filter_map(|s| {
            let name = s.get("artists")?.as_array()?.first()?.get("name")?.as_str()?;
            let name_lc = name.to_lowercase();
            let matches = artist_lc.contains(&name_lc) || name_lc.contains(&artist_lc);
            matches.then(|| s.get("id")?.as_i64()).flatten()
        })
        .find_map(|id| {
            let lyric_url = format!("https://music.163.com/api/song/lyric?id={id}&lv=1&kv=1&tv=-1");
            let doc: Value = serde_json::from_str(&netease_get(&lyric_url)?).ok()?;
            let lrc = doc.get("lrc")?.get("lyric")?.as_str()?;
            (!lrc.is_empty()).then(|| lrc.to_string())
        })
}

/// Fetch synced lyrics, trying lrclib first, then NetEase. Within lrclib the
/// exact `/api/get` and the looser `/api/search` fallback are issued
/// concurrently so the fallback adds no extra round-trip; the exact result wins
/// when it has synced lyrics. NetEase (a separate provider with wide coverage)
/// is only consulted when lrclib has nothing.
fn fetch_lrc(title: &str, artist: &str, album: &str, duration: f64) -> Option<String> {
    let mut get = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        urlencode(title),
        urlencode(artist),
    );
    if !album.is_empty() {
        get.push_str(&format!("&album_name={}", urlencode(album)));
    }
    if duration > 0.0 {
        get.push_str(&format!("&duration={}", duration.round() as i64));
    }
    let search = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        urlencode(title),
        urlencode(artist),
    );
    // Kick the search off concurrently so the fallback is already in flight,
    // but return the moment the exact get lands with lyrics (the search thread
    // is then just dropped) -- so a match costs one round-trip and a miss costs
    // one, never two in series.
    let search_handle = thread::spawn(move || curl_get(&search));
    if let Some(lrc) = curl_get(&get)
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .as_ref()
        .and_then(synced_from_object)
    {
        return Some(lrc);
    }
    if let Some(body) = search_handle.join().ok().flatten() {
        if let Some(lrc) = serde_json::from_str::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(Value::as_array)
            .and_then(|arr| arr.iter().find_map(synced_from_object))
        {
            return Some(lrc);
        }
    }
    // Nothing on lrclib -- fall back to NetEase.
    fetch_netease(title, artist)
}

/// Cap on the number of tracks kept in the on-disk lyric cache; the entries
/// with the lowest decayed play score (see `cache_score`) are evicted past
/// this.
const LYRIC_CACHE_MAX: usize = 1000;
/// Cap on the number of covers kept in the on-disk art cache. Covers are
/// ~30 KB each (vs a few KB per LRC), so this cap is tighter than the lyric
/// one to keep the cache in the low tens of MB.
const ART_CACHE_MAX: usize = 300;
/// Covers above this size are served but not cached: one oversized outlier
/// (curl allows up to 8M) would otherwise crowd out hundreds of thumbnails'
/// worth of budget. Refetching those is just the pre-cache status quo.
const ART_CACHE_ENTRY_MAX: usize = 1024 * 1024;

/// Whether the on-disk caches are used (config `MediaArtCache` /
/// `MediaLyricsCache`). Set from the main loop on config load. Disabling one
/// stops its reads and writes but leaves existing entries on disk, so
/// re-enabling picks the cache back up where it left off.
static ART_CACHE_ENABLED: AtomicBool = AtomicBool::new(true);
static LYRICS_CACHE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Apply the configured art-cache toggle; called on config load/reload.
pub fn set_art_cache(on: bool) {
    ART_CACHE_ENABLED.store(on, Ordering::Relaxed);
}

/// Apply the configured lyrics-cache toggle; called on config load/reload.
pub fn set_lyrics_cache(on: bool) {
    LYRICS_CACHE_ENABLED.store(on, Ordering::Relaxed);
}

/// An on-disk cache directory: `sub/` under systemd's $CACHE_DIRECTORY
/// (handed to the user on privilege drop), falling back to the user cache
/// dir when run outside the service -- the same chain widget scripts like
/// weather.sh use.
fn cache_dir(sub: &str) -> Option<PathBuf> {
    let base = std::env::var_os("CACHE_DIRECTORY")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(|d| PathBuf::from(d).join("not-quite-tiny-dfr"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/not-quite-tiny-dfr"))
        })?;
    Some(base.join(sub))
}

/// Stable cache filename for a key (FNV-1a; not `DefaultHasher`, whose
/// output may change across Rust releases and would orphan the whole cache).
/// The key itself is stored on the file's first line so a hash collision can
/// never serve another key's entry.
fn cache_file_name(key: &str, ext: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}.{ext}")
}

/// Serialize a cache entry: the key line, a `plays=N` line, then the data.
/// The play count feeds eviction; every cache hit rewrites the entry with the
/// count bumped (which also refreshes the mtime the decay is measured from).
fn cache_entry_body(key: &str, plays: u64, data: &[u8]) -> Vec<u8> {
    let header = format!("{key}\nplays={plays}\n");
    let mut body = Vec::with_capacity(header.len() + data.len());
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(data);
    body
}

/// Parse a cache entry into `(key, plays, data offset)`. Entries written
/// before play counts existed have no `plays=` line; they read as 0 plays
/// (first in line for eviction) with the data starting right after the key,
/// and the rewrite-on-hit upgrades them in place.
fn cache_parse_entry(body: &[u8]) -> Option<(&[u8], u64, usize)> {
    let nl = body.iter().position(|&b| b == b'\n')?;
    let key = &body[..nl];
    let rest = &body[nl + 1..];
    if let Some(r) = rest.strip_prefix(b"plays=") {
        if let Some(nl2) = r.iter().position(|&b| b == b'\n') {
            if let Ok(plays) = std::str::from_utf8(&r[..nl2]).unwrap_or("").parse() {
                return Some((key, plays, nl + 1 + 6 + nl2 + 1));
            }
        }
    }
    Some((key, 0, nl + 1))
}

/// The play count in an entry's header, reading only the leading bytes (art
/// entries can run to a MB, and eviction scans the whole directory). Anything
/// unparseable ranks as 0 plays.
fn cache_entry_plays(path: &Path) -> u64 {
    let mut buf = [0u8; 4096];
    let Ok(mut f) = fs::File::open(path) else { return 0 };
    let Ok(n) = f.read(&mut buf) else { return 0 };
    cache_parse_entry(&buf[..n]).map_or(0, |(_, plays, _)| plays)
}

/// Half-life of a play count for eviction ranking: 30 days without a play
/// halves an entry's score.
const CACHE_HALF_LIFE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Eviction score for an entry: its play count decayed by the time since the
/// last play (its mtime -- every hit rewrites the file). Lowest goes first.
/// The decay keeps a raw count from ruling forever: a track played 50 times
/// but untouched for a year scores ~0.01 and loses to anything heard this
/// week, while a current favorite still comfortably outranks one-offs.
fn cache_score(plays: u64, age: Duration) -> f64 {
    plays as f64 * 0.5f64.powf(age.as_secs_f64() / CACHE_HALF_LIFE.as_secs_f64())
}

/// Cached bytes for `key` in the `sub/` cache, if present. A hit counts as a
/// play: the entry is rewritten with its count bumped, so eviction can rank
/// by how often each track is actually played.
fn cache_load(sub: &str, ext: &str, key: &str) -> Option<Vec<u8>> {
    let path = cache_dir(sub)?.join(cache_file_name(key, ext));
    let body = fs::read(&path).ok()?;
    let (file_key, plays, data_at) = cache_parse_entry(&body)?;
    if file_key != key.as_bytes() {
        return None; // hash collision with a different key
    }
    let data = body[data_at..].to_vec();
    let _ = fs::write(&path, cache_entry_body(key, plays + 1, &data));
    Some(data)
}

/// Persist `data` for `key` in the `sub/` cache (one play so far), then evict
/// the entries with the lowest decayed play score beyond `max` (oldest first
/// among ties) -- so a track in steady rotation outlives a one-off heard
/// yesterday, but not by clinging to a play count it stopped earning.
fn cache_store(sub: &str, ext: &str, max: usize, key: &str, data: &[u8]) {
    let Some(dir) = cache_dir(sub) else { return };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = fs::write(
        dir.join(cache_file_name(key, ext)),
        cache_entry_body(key, 1, data),
    );
    let Ok(entries) = fs::read_dir(&dir) else { return };
    let now = SystemTime::now();
    let mut files: Vec<(f64, SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
            Some((cache_score(cache_entry_plays(&e.path()), age), mtime, e.path()))
        })
        .collect();
    if files.len() <= max {
        return;
    }
    files.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, _, path) in &files[..files.len() - max] {
        let _ = fs::remove_file(path);
    }
}

/// Cached LRC for the track, if present.
fn lyric_cache_load(track_key: &str) -> Option<String> {
    if !LYRICS_CACHE_ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    String::from_utf8(cache_load("lyrics", "lrc", track_key)?).ok()
}

/// Persist fetched LRC for the track. Only fetch hits are stored: a miss is
/// retried over the network next play, so lyrics added to the providers
/// later still show up.
fn lyric_cache_store(track_key: &str, lrc: &str) {
    if !LYRICS_CACHE_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    cache_store("lyrics", "lrc", LYRIC_CACHE_MAX, track_key, lrc.as_bytes());
}

/// Cached cover bytes (as fetched, still encoded) for an art URL, if present.
fn art_cache_load(url: &str) -> Option<Vec<u8>> {
    if !ART_CACHE_ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    cache_load("art", "art", url)
}

/// Persist fetched cover bytes for an art URL. Only bytes that already
/// decoded are stored, so a truncated download or an error page can never
/// poison the cache; oversized covers are skipped per `ART_CACHE_ENTRY_MAX`.
fn art_cache_store(url: &str, bytes: &[u8]) {
    if !ART_CACHE_ENABLED.load(Ordering::Relaxed) || bytes.len() > ART_CACHE_ENTRY_MAX {
        return;
    }
    cache_store("art", "art", ART_CACHE_MAX, url, bytes);
}

/// Spawn a background fetch of lyrics for `track_key`, publishing the parsed
/// result into `LYRICS_STATE` only if that track is still current.
fn fetch_lyrics(
    track_key: String,
    title: String,
    artist: String,
    album: String,
    duration: f64,
    wake: Arc<OwnedFd>,
) {
    thread::spawn(move || {
        // The on-disk cache first (works offline, saves a round-trip on
        // replays); on a miss, fetch and cache the hit for next time.
        let lrc = lyric_cache_load(&track_key).or_else(|| {
            let fetched = fetch_lrc(&title, &artist, &album, duration);
            if let Some(lrc) = &fetched {
                lyric_cache_store(&track_key, lrc);
            }
            fetched
        });
        let lines = lrc.map(|lrc| parse_lrc(&lrc)).unwrap_or_default();
        let mut ly = LYRICS_STATE.lock().unwrap();
        if ly.track_key != track_key {
            return; // the track changed while we were fetching
        }
        ly.status = if lines.is_empty() {
            LyricsStatus::None
        } else {
            LyricsStatus::Synced
        };
        // Seed the display at the line matching the current playback position,
        // not line 0 -- otherwise the view flashes the first line and then
        // immediately slides to the real one when the sync thread catches up.
        let pos = derived_position().unwrap_or(0.0);
        ly.current = index_for_time(&lines, pos).unwrap_or(0);
        // Adopt the real gap state for the current position. Forcing "out of a
        // gap" here flashed the lyrics on for a load mid-break (intro /
        // instrumental) and then hid them a moment later once the sync thread's
        // debounce elapsed; seeding the true state settles straight to whichever
        // view is correct. The sync thread pre-satisfies its debounce on this
        // fresh load so it doesn't reverse the decision.
        ly.in_gap = gap_at(&lines, pos);
        ly.lines = lines;
        ly.generation = ly.generation.wrapping_add(1);
        notify(&wake);
    });
}

/// Position anchor for lyric sync: `(position_secs, read_at, playing)`. Updated
/// by the metadata thread; read by the lyric-sync thread to derive the current
/// position without another playerctl call.
static ANCHOR: Mutex<Option<(f64, Instant, bool)>> = Mutex::new(None);

/// User lyric-timing offset in milliseconds (config `LyricOffset`, in seconds).
/// Added to the derived playback position before looking up the current line, so
/// a positive value shows lyrics earlier (compensating for audio output latency)
/// and a negative value shows them later. Set from the main loop on config load.
static LYRIC_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

/// Apply the configured lyric offset (seconds); called on config load/reload.
pub fn set_lyric_offset(secs: f64) {
    LYRIC_OFFSET_MS.store((secs * 1000.0).round() as i64, Ordering::Relaxed);
}

/// Whether the album cover is blurred behind the panel (config `MediaCoverBlur`).
/// Read by the render thread when it builds the cover surface. Set from the main
/// loop on config load.
static COVER_BLUR: AtomicBool = AtomicBool::new(false);

/// Apply the configured cover-blur toggle; called on config load/reload.
pub fn set_cover_blur(on: bool) {
    COVER_BLUR.store(on, Ordering::Relaxed);
}

/// Whether the album cover should be drawn blurred.
pub fn cover_blur() -> bool {
    COVER_BLUR.load(Ordering::Relaxed)
}

/// Current playback position derived from the anchor, in seconds; `None` when no
/// anchor has been set yet. Extrapolates while playing, and shifts by the
/// configured `LyricOffset` so the lyric lookup leads or trails the audio.
fn derived_position() -> Option<f64> {
    let offset = LYRIC_OFFSET_MS.load(Ordering::Relaxed) as f64 / 1000.0;
    match *ANCHOR.lock().unwrap() {
        Some((pos, at, true)) => Some(pos + at.elapsed().as_secs_f64() + offset),
        Some((pos, _, false)) => Some(pos + offset),
        None => None,
    }
}

/// Spawn the media threads. `wake` is the write end of the loop's wake pipe.
///
/// Two threads so the lyric line never frame-starves: a metadata thread that
/// runs the heavier playerctl query every `POLL`, and a lyric-sync thread that
/// only does arithmetic every `LYRIC_TICK` -- so a slow playerctl call can't
/// stall the highlighted line's advance.
pub fn spawn_poller(wake: Arc<OwnedFd>) {
    spawn_metadata_thread(wake.clone());
    spawn_lyric_sync_thread(wake);
}

fn spawn_metadata_thread(wake: Arc<OwnedFd>) {
    thread::spawn(move || {
        // The art URL currently decoded into MEDIA_STATE, so unchanged art is
        // never re-decoded. Only recorded on a successful decode; failures go
        // through `art_retry` instead.
        let mut cur_art_url = String::new();
        // Backoff state for a failing art URL: (url, next attempt, its delay).
        let mut art_retry: Option<(String, Instant, Duration)> = None;
        // The track lyrics are currently loaded/fetching for.
        let mut cur_track_key = String::new();
        loop {
            let track = query();
            let mut status = track.status;
            // YouTube ad guard. During an ad YouTube swaps the session's URL to
            // plain `youtube.com` (no `watch?v=<id>`) while still reporting
            // "Playing" with the ad's title -- and sometimes its own art. A real
            // video always carries a video id, so a YouTube-host URL with none
            // is an ad (or the homepage autoplaying): report it inactive to keep
            // the panel collapsed.
            let is_youtube_ad = is_ad_url(&track.page_url);
            if is_youtube_ad {
                status = MediaStatus::Idle;
            }
            // The art source: a stable YouTube thumbnail derived from the page
            // URL when applicable, else the player's own art URL. Skipped for an
            // ad. Only (re)load for a new, non-empty source -- browsers flap
            // `mpris:artUrl` to "" mid-track, and clearing on those blanks
            // flickered the cover to black, so an empty source is ignored and
            // the current art kept.
            let art_source = if is_youtube_ad {
                String::new()
            } else {
                art_source_url(&track.art_url, &track.page_url)
            };
            // Attempt a load only while the player is active: during Idle
            // (e.g. Stopped at startup or between tracks) the metadata is not
            // settled yet, and burning the attempt there used to lose the
            // cover for the whole track. Failed URLs are retried with backoff.
            let want_art =
                status != MediaStatus::Idle && !art_source.is_empty() && art_source != cur_art_url;
            let art_attempt = want_art
                && match &art_retry {
                    Some((url, next, _)) if *url == art_source => Instant::now() >= *next,
                    _ => true, // first try for this URL
                };
            let decoded = if art_attempt { load_art(&art_source) } else { None };
            let art_ok = decoded.is_some();
            // On the first failure for a new URL, clear the previous track's
            // cover (retries then leave the blank panel alone).
            let art_clear = art_attempt
                && !art_ok
                && !matches!(&art_retry, Some((url, _, _)) if *url == art_source);
            if art_attempt {
                if art_ok {
                    cur_art_url = art_source.clone();
                    art_retry = None;
                } else {
                    let delay = match &art_retry {
                        Some((url, _, d)) if *url == art_source => (*d * 2).min(ART_RETRY_MAX),
                        _ => ART_RETRY_MIN,
                    };
                    art_retry = Some((art_source.clone(), Instant::now() + delay, delay));
                }
            }
            {
                let mut shared = MEDIA_STATE.lock().unwrap();
                let meta_changed = shared.status != status
                    || shared.title != track.title
                    || shared.artist != track.artist;
                if art_ok {
                    shared.art = decoded;
                } else if art_clear {
                    shared.art = None;
                }
                if meta_changed || art_ok || art_clear {
                    shared.status = status;
                    shared.title = track.title.clone();
                    shared.artist = track.artist.clone();
                    shared.generation = shared.generation.wrapping_add(1);
                    notify(&wake);
                }
            }
            // Re-anchor the position for the lyric-sync thread.
            *ANCHOR.lock().unwrap() = Some((
                track.position,
                Instant::now(),
                status == MediaStatus::Playing,
            ));
            // On a real-track change, (re)fetch lyrics.
            let real = status != MediaStatus::Idle && !track.title.is_empty();
            let track_key = if real {
                format!("{}\u{1f}{}", track.title, track.artist)
            } else {
                String::new()
            };
            if track_key != cur_track_key {
                cur_track_key = track_key.clone();
                {
                    let mut ly = LYRICS_STATE.lock().unwrap();
                    ly.track_key = track_key.clone();
                    ly.lines.clear();
                    ly.current = 0;
                    ly.in_gap = false;
                    ly.status = if real {
                        LyricsStatus::Loading
                    } else {
                        LyricsStatus::None
                    };
                    ly.generation = ly.generation.wrapping_add(1);
                    notify(&wake);
                }
                if real {
                    fetch_lyrics(
                        track_key,
                        track.title,
                        track.artist,
                        track.album,
                        track.length,
                        wake.clone(),
                    );
                }
            }
            thread::sleep(POLL);
        }
    });
}

fn spawn_lyric_sync_thread(wake: Arc<OwnedFd>) {
    thread::spawn(move || {
        // Playback position at which the current gap began, so a gap only flips
        // the view once it has lasted `GAP_DEBOUNCE` seconds of playback. Reset
        // on a backward jump (a seek, or a new track starting near 0) so each
        // gap is timed from its own start rather than the previous one's.
        let mut gap_start: Option<f64> = None;
        // Monotonic playback position. playerctl re-anchors can report a spot
        // slightly behind the extrapolated one; a backward dip across a line
        // boundary would bounce the highlighted line (b -> a -> b). Hold the
        // position against small backward jitter; only a large drop (a real
        // seek, or a new track) moves it back.
        let mut mono_pos: Option<f64> = None;
        // Whether lyrics were present on the previous tick, so a fresh load
        // (none -> synced) can be detected and its debounce pre-satisfied.
        let mut had_lyrics = false;
        loop {
            // Derive the current position from the anchor (no playerctl here).
            if let Some(derived) = derived_position() {
                let pos = match mono_pos {
                    Some(prev) if derived + SEEK_BACK_THRESHOLD >= prev => derived.max(prev),
                    _ => derived, // first read, or a real backward seek
                };
                mono_pos = Some(pos);
                let mut ly = LYRICS_STATE.lock().unwrap();
                if ly.has_lyrics() {
                    // On a fresh load, adopt the gap state `fetch_lyrics` seeded
                    // rather than debouncing into it: pre-satisfy the debounce so
                    // lyrics that land during an intro/break stay on the controls
                    // instead of flashing on for `GAP_DEBOUNCE` and hiding again.
                    if !had_lyrics {
                        had_lyrics = true;
                        gap_start = gap_at(&ly.lines, pos).then_some(pos - GAP_DEBOUNCE);
                    }
                    let mut changed = false;
                    if let Some(idx) = index_for_time(&ly.lines, pos) {
                        if ly.current != idx {
                            ly.current = idx;
                            changed = true;
                        }
                    }
                    // Debounce the gap: report it only once the position has sat
                    // in a break for `GAP_DEBOUNCE` seconds; clear it at once
                    // when a lyric becomes active again.
                    let gap = if gap_at(&ly.lines, pos) {
                        let start = match gap_start {
                            Some(s) if pos >= s => s, // same gap, still running
                            _ => {
                                gap_start = Some(pos); // new gap (or seeked back)
                                pos
                            }
                        };
                        pos - start >= GAP_DEBOUNCE
                    } else {
                        gap_start = None;
                        false
                    };
                    if ly.in_gap != gap {
                        ly.in_gap = gap;
                        changed = true;
                    }
                    if changed {
                        ly.generation = ly.generation.wrapping_add(1);
                        notify(&wake);
                    }
                } else {
                    // No lyrics (track changed / still fetching): the next load
                    // counts as fresh.
                    had_lyrics = false;
                    gap_start = None;
                }
            }
            thread::sleep(LYRIC_TICK);
        }
    });
}

/// One player's current state, as parsed from a playerctl format line.
#[derive(Default)]
struct Track {
    status: MediaStatus,
    title: String,
    artist: String,
    art_url: String,
    page_url: String,
    /// Playback position and track length, in seconds (0 when unavailable).
    position: f64,
    length: f64,
    album: String,
}

const PLAYERCTL_FORMAT: &str = "{{status}}\u{1f}{{xesam:title}}\u{1f}{{xesam:artist}}\u{1f}{{mpris:artUrl}}\u{1f}{{xesam:url}}\u{1f}{{position}}\u{1f}{{mpris:length}}\u{1f}{{xesam:album}}";

/// Which player to follow. The most-recently-active player (`playerctld`) is
/// preferred, but a paused most-recent player yields to one that is actually
/// playing -- so starting playback in another app switches to it. Empty strings
/// when there is no player.
fn query() -> Track {
    let latest = run_query(&["-p", "playerctld"]);
    // Most-recent player is itself playing a real track: follow it.
    let latest_playing = latest
        .as_ref()
        .is_some_and(|t| t.status == MediaStatus::Playing && !is_ad_url(&t.page_url));
    if latest_playing {
        return latest.unwrap();
    }
    // It's paused/idle (or on an ad): switch to whatever is actually playing.
    if let Some(playing) = first_playing() {
        return playing;
    }
    // Nothing playing: show the most-recent (e.g. a paused track), else the
    // default pick, else idle.
    latest.or_else(|| run_query(&[])).unwrap_or_default()
}

/// A YouTube ad: a YouTube-host URL with no video id (real videos always carry
/// one; YouTube swaps the URL to plain youtube.com during ads).
fn is_ad_url(url: &str) -> bool {
    is_youtube_host(url) && youtube_id(url).is_none()
}

/// Parse one playerctl format line (see `PLAYERCTL_FORMAT`).
fn parse_line(line: &str) -> Track {
    let mut parts = line.splitn(8, '\u{1f}');
    let status = match parts.next().unwrap_or("") {
        "Playing" => MediaStatus::Playing,
        "Paused" => MediaStatus::Paused,
        _ => MediaStatus::Idle,
    };
    let title = parts.next().unwrap_or("").to_string();
    let artist = parts.next().unwrap_or("").to_string();
    let art_url = parts.next().unwrap_or("").to_string();
    let page_url = parts.next().unwrap_or("").to_string();
    // Position and length arrive in microseconds.
    let position = parts.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e6;
    let length = parts.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) / 1e6;
    let album = parts.next().unwrap_or("").to_string();
    Track {
        status,
        title,
        artist,
        art_url,
        page_url,
        position,
        length,
        album,
    }
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
        if track.status != MediaStatus::Playing {
            continue;
        }
        let rank = if is_ad_url(&track.page_url) { 1 } else { 2 };
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
/// thumbnails, fetched with curl). Returns `None` for an empty or unsupported
/// URL, or any fetch/decode failure -- the widget then draws a plain panel
/// (and the poller retries with backoff).
///
/// Fetched covers go through the on-disk art cache (works offline, saves the
/// round-trip on replays); `file://` art is already local, so only the http
/// path is cached.
fn load_art(url: &str) -> Option<MediaArt> {
    if url.is_empty() {
        return None;
    }
    if let Some(path) = url.strip_prefix("file://") {
        return decode_art(&std::fs::read(percent_decode(path)).ok()?);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    // A cached entry that no longer decodes (corrupt file) falls through to a
    // fresh fetch, whose store then overwrites it.
    if let Some(art) = art_cache_load(url).and_then(|bytes| decode_art(&bytes)) {
        return Some(art);
    }
    let bytes = fetch_url(url)?;
    let art = decode_art(&bytes)?;
    art_cache_store(url, &bytes);
    Some(art)
}

/// Decode encoded image bytes into a cairo ARGB32 buffer, downscaled to
/// `ART_MAX`.
fn decode_art(bytes: &[u8]) -> Option<MediaArt> {
    let img = image::load_from_memory(bytes).ok()?.thumbnail(ART_MAX, ART_MAX);
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
/// a slow or huge response can't stall the poller or balloon memory. `-f` makes
/// an HTTP error status count as a failure instead of handing the error page
/// to the image decoder.
fn fetch_url(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-sfL", "--max-time", "5", "--max-filesize", "8M", url])
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

#[cfg(test)]
mod tests {
    use super::*;

    // One test (not several) because it points the caches at a scratch dir via
    // CACHE_DIRECTORY, and parallel tests would race on the process env.
    #[test]
    fn disk_cache_roundtrip_collision_and_eviction() {
        let dir = std::env::temp_dir().join(format!("nqtd-disk-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("CACHE_DIRECTORY", &dir);

        let key = "Title\u{1f}Artist";
        let lrc = "[00:01.00] hello\n[00:02.00] world";
        assert_eq!(lyric_cache_load(key), None);
        lyric_cache_store(key, lrc);
        assert_eq!(lyric_cache_load(key).as_deref(), Some(lrc));

        // A file under this key's name but holding another track's key (hash
        // collision) must not be served.
        let path = dir.join("lyrics").join(cache_file_name(key, "lrc"));
        fs::write(&path, "Other\u{1f}Someone\nwrong").unwrap();
        assert_eq!(lyric_cache_load(key), None);

        // Fill to the cap with old entries; the next store evicts down to the
        // cap and the fresh entry survives.
        let lyrics_dir = dir.join("lyrics");
        let old = SystemTime::now() - Duration::from_secs(3600);
        for i in 0..LYRIC_CACHE_MAX {
            let p = lyrics_dir.join(format!("{i:016x}.old"));
            fs::write(&p, "x").unwrap();
            fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let fresh = "New\u{1f}Band";
        lyric_cache_store(fresh, lrc);
        assert_eq!(fs::read_dir(&lyrics_dir).unwrap().count(), LYRIC_CACHE_MAX);
        assert_eq!(lyric_cache_load(fresh).as_deref(), Some(lrc));

        // Play count outranks recency: `fresh` has been played several times,
        // so even as the oldest file in the directory it survives eviction
        // while a never-played (0-play) filler goes instead.
        assert_eq!(lyric_cache_load(fresh).as_deref(), Some(lrc));
        let fresh_path = lyrics_dir.join(cache_file_name(fresh, "lrc"));
        fs::File::options()
            .write(true)
            .open(&fresh_path)
            .unwrap()
            .set_modified(old - Duration::from_secs(3600))
            .unwrap();
        lyric_cache_store("Encore\u{1f}Band", lrc);
        assert_eq!(fs::read_dir(&lyrics_dir).unwrap().count(), LYRIC_CACHE_MAX);
        assert_eq!(lyric_cache_load(fresh).as_deref(), Some(lrc));

        // A pre-play-count entry (no `plays=` line) still loads, and the hit
        // rewrites it into the counted format.
        let legacy = "Legacy\u{1f}Old";
        let legacy_path = lyrics_dir.join(cache_file_name(legacy, "lrc"));
        fs::write(&legacy_path, format!("{legacy}\n{lrc}")).unwrap();
        assert_eq!(lyric_cache_load(legacy).as_deref(), Some(lrc));
        assert_eq!(cache_entry_plays(&legacy_path), 1);

        // Art cache: binary bytes (including embedded newlines and non-UTF-8)
        // round-trip intact, and oversized covers are not stored.
        let url = "https://img.youtube.com/vi/abcdefghijk/hqdefault.jpg";
        let cover = [0xffu8, 0xd8, 0xff, b'\n', 0x00, b'\n', 0xfe];
        assert_eq!(art_cache_load(url), None);
        art_cache_store(url, &cover);
        assert_eq!(art_cache_load(url).as_deref(), Some(&cover[..]));
        let big_url = "https://example.com/huge.png";
        art_cache_store(big_url, &vec![0u8; ART_CACHE_ENTRY_MAX + 1]);
        assert_eq!(art_cache_load(big_url), None);

        // Time decay: `url` has 2 plays but is backdated a year, so once the
        // art cache is over the cap it loses eviction to the just-stored
        // 1-play covers despite its higher raw count.
        let art_dir = dir.join("art");
        fs::File::options()
            .write(true)
            .open(art_dir.join(cache_file_name(url, "art")))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(365 * 24 * 3600))
            .unwrap();
        for i in 0..ART_CACHE_MAX {
            art_cache_store(&format!("https://example.com/{i}.jpg"), &cover);
        }
        assert_eq!(fs::read_dir(&art_dir).unwrap().count(), ART_CACHE_MAX);
        assert_eq!(art_cache_load(url), None);

        // Disabled caches neither serve existing entries nor store new ones,
        // but the entries themselves survive for when the cache is re-enabled.
        set_lyrics_cache(false);
        set_art_cache(false);
        assert_eq!(lyric_cache_load(fresh), None);
        lyric_cache_store("Gated\u{1f}Band", lrc);
        assert_eq!(art_cache_load("https://example.com/0.jpg"), None);
        art_cache_store("https://example.com/gated.jpg", &cover);
        set_lyrics_cache(true);
        set_art_cache(true);
        assert_eq!(lyric_cache_load(fresh).as_deref(), Some(lrc));
        assert_eq!(lyric_cache_load("Gated\u{1f}Band"), None);
        assert_eq!(art_cache_load("https://example.com/gated.jpg"), None);

        let _ = fs::remove_dir_all(&dir);
        std::env::remove_var("CACHE_DIRECTORY");
    }

    #[test]
    fn decayed_score_ranks_frequency_and_age() {
        // One half-life halves the count; fresh entries keep the raw count.
        assert!((cache_score(4, CACHE_HALF_LIFE) - 2.0).abs() < 1e-9);
        assert!((cache_score(3, Duration::ZERO) - 3.0).abs() < 1e-9);
        // An old favorite untouched for a year loses to anything played now,
        // but a current favorite still outranks a fresh one-off.
        let year = Duration::from_secs(365 * 24 * 3600);
        assert!(cache_score(50, year) < cache_score(1, Duration::ZERO));
        assert!(cache_score(50, Duration::from_secs(24 * 3600)) > cache_score(1, Duration::ZERO));
        // Never-played (legacy/unparseable) entries rank below everything.
        assert_eq!(cache_score(0, Duration::ZERO), 0.0);
    }
}
