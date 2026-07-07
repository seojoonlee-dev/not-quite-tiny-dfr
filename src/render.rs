//! Rendering primitives and the media-panel painter.
//!
//! Pure cairo/pango drawing helpers shared across the button and layer render
//! paths, plus the album-cover media panel (transport controls, track text,
//! and timed lyrics) driven by the `media` backend's `MEDIA_STATE` /
//! `LYRICS_STATE`.

use crate::media::{self, MediaStatus, LYRICS_STATE, MEDIA_STATE};
use crate::pixel_shift::{PIXEL_SHIFT_HEIGHT_PX, PIXEL_SHIFT_WIDTH_PX};
use crate::style::{self, Color};
use crate::{
    ease_expand, CachedSvg, MEDIA_COVER_DARKEN, MEDIA_ICON_PAD, MEDIA_LYRIC_ANIM, MEDIA_PAD,
    MEDIA_TEXT_GAP, MEDIA_TEXT_VPAD, MEDIA_VIEW_ANIM,
};
use cairo::{Context, Format, ImageSurface};
use std::time::Instant;

/// The three transport controls of a Media widget, left to right. Also the
/// tap-zone identity: the widget's width is split into three equal columns.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaZone {
    Prev,
    PlayPause,
    Next,
}

impl MediaZone {
    /// The playerctl verb this control runs.
    pub(crate) fn verb(self) -> &'static str {
        match self {
            MediaZone::Prev => "previous",
            MediaZone::PlayPause => "play-pause",
            MediaZone::Next => "next",
        }
    }
}

/// A media widget: its own transport icons, plus a press highlight. The live
/// playback status, track text, and album art come from the global
/// `MEDIA_STATE`, and timed lyrics from `LYRICS_STATE` (see the `media`
/// module), not from here.
pub(crate) struct MediaState {
    pub(crate) prev_icon: Option<CachedSvg>,
    pub(crate) play_icon: Option<CachedSvg>,
    pub(crate) pause_icon: Option<CachedSvg>,
    pub(crate) next_icon: Option<CachedSvg>,
    /// The zone under a finger right now, drawn brighter for tap feedback.
    pub(crate) pressed: Option<MediaZone>,
    /// When lyrics are available, whether to show them (vs the transport row).
    /// Reset to true for each new track (`lyrics_track`); tapping toggles it.
    pub(crate) show_lyrics: bool,
    /// Whether there is no lyric to show right now: no lyrics for the track, or
    /// the playback position sits in a gap (intro / blank instrumental line).
    /// Forces the controls/title view regardless of `show_lyrics`, cross-fading
    /// through `view_anim` like the manual toggle.
    pub(crate) lyric_gap: bool,
    /// The track (title) `show_lyrics` was last reset for.
    pub(crate) lyrics_track: String,
    /// Start of the lyrics/controls cross-fade; `None` once settled.
    pub(crate) view_anim: Option<Instant>,
    /// Last tap, for auto-returning from the transport row to the lyrics.
    pub(crate) last_interaction: Instant,
    /// The lyric line index currently displayed, and the outgoing line and
    /// start of the vertical slide when it advances (`None` once settled).
    pub(crate) lyric_idx: usize,
    pub(crate) prev_lyric: String,
    pub(crate) lyric_anim: Option<Instant>,
}

impl MediaState {
    /// How much the transport row is shown vs the lyrics: 0 = lyrics, 1 =
    /// controls, cross-fading across `MEDIA_VIEW_ANIM`.
    fn controls_alpha(&self) -> f64 {
        // The controls win when the user has toggled to them OR there is no
        // lyric to show right now (no lyrics / a gap).
        let want_controls = !self.show_lyrics || self.lyric_gap;
        let settled = if want_controls { 1.0 } else { 0.0 };
        let Some(t0) = self.view_anim else {
            return settled;
        };
        let t = t0.elapsed().as_secs_f64() / MEDIA_VIEW_ANIM.as_secs_f64();
        if t >= 1.0 {
            return settled;
        }
        // Ease and travel from the opposite end toward the settled value.
        let eased = ease_expand(t.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        if want_controls {
            eased
        } else {
            1.0 - eased
        }
    }

    /// Progress of the lyric-line slide, 0 (just changed) to 1 (settled).
    fn lyric_progress(&self) -> f64 {
        let Some(t0) = self.lyric_anim else {
            return 1.0;
        };
        (t0.elapsed().as_secs_f64() / MEDIA_LYRIC_ANIM.as_secs_f64()).clamp(0.0, 1.0)
    }
}

/// Set the cairo source to the background image (positioned to fill the bar) if
/// one is configured, otherwise the solid background color. `shift` is the
/// pixel-shift offset: the image is loaded PIXEL_SHIFT_* px larger than the bar
/// so it can slide around without exposing its edges.
pub(crate) fn set_background_source(c: &Context, style: &style::Style, shift: (f64, f64)) {
    if let Some(img) = &style.background_image {
        c.set_source_surface(
            img,
            shift.0 - (PIXEL_SHIFT_WIDTH_PX / 2) as f64,
            shift.1 - (PIXEL_SHIFT_HEIGHT_PX / 2) as f64,
        )
        .unwrap();
    } else {
        style.background.set_source(c);
    }
}

/// Fill a horizontal capsule (a rectangle with fully rounded ends).
pub(crate) fn capsule(c: &Context, x: f64, y: f64, w: f64, h: f64) {
    let r = (h / 2.0).min(w / 2.0);
    c.new_sub_path();
    c.arc(x + w - r, y + r, r, (-90.0f64).to_radians(), (90.0f64).to_radians());
    c.arc(x + r, y + r, r, (90.0f64).to_radians(), (270.0f64).to_radians());
    c.close_path();
    c.fill().unwrap();
}

/// Lay `text` out in the bar font. Pango shapes with per-glyph font fallback
/// (and color emoji), which the cairo toy API's single font face could not.
pub(crate) fn text_layout(c: &Context, style: &style::Style, text: &str) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(c);
    layout.set_font_description(Some(&style.font));
    layout.set_text(text);
    layout
}

/// Draw `layout` horizontally centered in the button and vertically centered
/// in the bar, with the cairo source as the text color.
pub(crate) fn show_layout_centered(
    c: &Context,
    layout: &pango::Layout,
    height: i32,
    button_left_edge: f64,
    button_width: u64,
    y_shift: f64,
) {
    let (tw, th) = layout.pixel_size();
    c.move_to(
        button_left_edge + (button_width as f64 / 2.0 - tw as f64 / 2.0).round(),
        y_shift + ((height as f64 - th as f64) / 2.0).round(),
    );
    pangocairo::functions::show_layout(c, layout);
}

/// Append a rounded-rectangle sub-path spanning `[left_edge, left_edge+width]`
/// across the button band `[bot, top]`. The corner centers are inset by the
/// radius so the rounding stays inside the short panel.
pub(crate) fn rounded_rect_path(c: &Context, left_edge: f64, width: f64, radius: f64, bot: f64, top: f64) {
    c.new_sub_path();
    let left = left_edge + radius;
    let right = (left_edge + width.ceil()) - radius;
    let cy_top = bot + radius;
    let cy_bot = top - radius;
    c.arc(right, cy_top, radius, (-90.0f64).to_radians(), 0.0);
    c.arc(right, cy_bot, radius, 0.0, (90.0f64).to_radians());
    c.arc(left, cy_bot, radius, (90.0f64).to_radians(), (180.0f64).to_radians());
    c.arc(left, cy_top, radius, (180.0f64).to_radians(), (270.0f64).to_radians());
    c.close_path();
}

/// The transport-control layout for a media panel: `(first_zone_left,
/// zone_width)`. Active (now-playing) clusters the three controls on the right
/// so the album text has the left; idle spreads them across the full width.
/// Shared by rendering and hit-testing so taps land on the drawn icons.
pub(crate) fn media_zone_geom(active: bool, left_edge: f64, button_width: f64, icon_w: f64) -> (f64, f64) {
    if active {
        let zone_w = icon_w + MEDIA_ICON_PAD * 2.0;
        let cluster = (zone_w * 3.0).min(button_width);
        (left_edge + button_width - cluster - MEDIA_PAD, cluster / 3.0)
    } else {
        (left_edge, button_width / 3.0)
    }
}

/// Which transport zone a touch at panel-relative x falls in, or `None` for the
/// inert text area of an active panel.
pub(crate) fn media_zone_at(active: bool, x: f64, left_edge: f64, button_width: f64, icon_w: f64) -> Option<MediaZone> {
    let (first, zone_w) = media_zone_geom(active, left_edge, button_width, icon_w);
    if zone_w <= 0.0 || x < first || x >= first + zone_w * 3.0 {
        return None;
    }
    Some(match ((x - first) / zone_w) as usize {
        0 => MediaZone::Prev,
        1 => MediaZone::PlayPause,
        _ => MediaZone::Next,
    })
}

/// Build (and thread-locally cache) the cairo surface for the current album
/// art. Cairo surfaces are not `Send`, so the raw bytes are published by the
/// poller and wrapped here on the render thread, rebuilt only when the media
/// generation moves.
pub(crate) fn media_art_surface(info: &media::MediaInfo) -> Option<ImageSurface> {
    thread_local! {
        // (media generation, blur applied) -> cached surface. The blur flag is
        // part of the key so toggling MediaCoverBlur live rebuilds the surface
        // even though the track (generation) has not changed.
        static CACHE: std::cell::RefCell<Option<(u64, bool, Option<ImageSurface>)>> =
            const { std::cell::RefCell::new(None) };
    }
    let blur = media::cover_blur();
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.as_ref().map(|(g, b, _)| (*g, *b)) != Some((info.generation, blur)) {
            let built = info.art.as_ref().and_then(|a| {
                let mut data = a.data.clone();
                if blur {
                    let (w, h) = (a.width.max(0) as usize, a.height.max(0) as usize);
                    // Radius scaled to the cover so it reads as a blur at any art
                    // size (covers are downscaled to <=256px on their long side).
                    let radius = (w.min(h) / 32).clamp(2, 10);
                    box_blur_argb32(&mut data, w, h, a.stride.max(0) as usize, radius);
                }
                ImageSurface::create_for_data(data, Format::ARgb32, a.width, a.height, a.stride).ok()
            });
            *cache = Some((info.generation, blur, built));
        }
        cache.as_ref().and_then(|(_, _, s)| s.clone())
    })
}

/// Blur an ARGB32 pixel buffer in place with a separable box blur, repeated three
/// times to approximate a Gaussian. `stride` is the row length in bytes (>= 4*w);
/// edges are extended (clamped) so the blur does not darken toward the borders.
/// Channels are blurred independently, which is correct for the premultiplied
/// ARGB the cover is stored as.
pub(crate) fn box_blur_argb32(data: &mut [u8], w: usize, h: usize, stride: usize, radius: usize) {
    if radius == 0 || w < 2 || h < 2 {
        return;
    }
    let mut tmp = data.to_vec();
    for _ in 0..3 {
        // Horizontal: each row is a line of `w` pixels, 4 bytes apart.
        blur_1d(data, &mut tmp, h, stride, w, 4, radius);
        // Vertical: each column is a line of `h` pixels, `stride` bytes apart.
        blur_1d(&tmp, data, w, 4, h, stride, radius);
    }
}

/// One separable box-blur pass. Reads `src`, writes `dst`. There are `lines`
/// parallel lines starting `line_step` bytes apart; each has `len` samples
/// spaced `elem_step` bytes apart. Runs a moving sum so the cost is independent
/// of the radius.
#[allow(clippy::too_many_arguments)]
pub(crate) fn blur_1d(
    src: &[u8],
    dst: &mut [u8],
    lines: usize,
    line_step: usize,
    len: usize,
    elem_step: usize,
    r: usize,
) {
    let denom = (2 * r + 1) as u32;
    for line in 0..lines {
        let base = line * line_step;
        for ch in 0..4 {
            let at = |k: usize| src[base + k * elem_step + ch] as u32;
            // Window centered at index 0, with the left overhang clamped to the
            // first sample.
            let mut sum = r as u32 * at(0);
            for k in 0..=r {
                sum += at(k.min(len - 1));
            }
            for i in 0..len {
                dst[base + i * elem_step + ch] = (sum / denom) as u8;
                let out_k = i.saturating_sub(r);
                let in_k = (i + 1 + r).min(len - 1);
                sum = sum - at(out_k) + at(in_k);
            }
        }
    }
}

/// Paint the media widget across its full span: a now-playing panel (album
/// cover, darkened, with the track text and transport controls) while a player
/// is active, or an idle transport row otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_media(
    c: &Context,
    m: &MediaState,
    style: &style::Style,
    text_color: Color,
    icon_w: f64,
    left_edge: f64,
    button_width: f64,
    radius: f64,
    bot: f64,
    top: f64,
    height: i32,
    y_shift: f64,
) {
    let info = MEDIA_STATE.lock().unwrap();
    let active = info.is_active();

    if active {
        // Panel background: a neutral dark base, the album cover on top
        // (cover-fit, clipped), and -- only when there IS art -- a darkening
        // pass so the white text/icons stay legible. Without art (e.g. a
        // browser that publishes no thumbnail) the base is left as a plain
        // dark-gray panel rather than being darkened to near-black.
        c.save().unwrap();
        rounded_rect_path(c, left_edge, button_width, radius, bot, top);
        c.clip();
        Color { r: 0.14, g: 0.14, b: 0.14, a: 1.0 }.set_source(c);
        c.paint().unwrap();
        let has_art = if let Some(surface) = media_art_surface(&info) {
            let (iw, ih) = (surface.width() as f64, surface.height() as f64);
            let (pw, ph) = (button_width, top - bot);
            let scale = (pw / iw).max(ph / ih);
            c.save().unwrap();
            c.translate(left_edge + pw / 2.0, bot + ph / 2.0);
            c.scale(scale, scale);
            c.set_source_surface(&surface, -iw / 2.0, -ih / 2.0).unwrap();
            c.source().set_extend(cairo::Extend::Pad);
            c.paint().unwrap();
            c.restore().unwrap();
            true
        } else {
            false
        };
        if has_art {
            Color { r: 0.0, g: 0.0, b: 0.0, a: MEDIA_COVER_DARKEN }.set_source(c);
            c.paint().unwrap();
        }
        c.restore().unwrap();

        // The currently-active lyric line (empty during a gap), if the track
        // has lyrics at all.
        let (has_lyrics, current_line) = {
            let ly = LYRICS_STATE.lock().unwrap();
            if ly.has_lyrics() {
                let i = ly.current.min(ly.lines.len() - 1);
                (true, ly.lines.get(i).map(|l| l.1.clone()).unwrap_or_default())
            } else {
                (false, String::new())
            }
        };
        // Cross-fade the lyrics (0) and transport row (1). The `lyric_gap` state
        // (set from the poller) already folds "no lyrics" and "in a gap" into
        // `controls_alpha`, so the row wins in those cases.
        let controls_alpha = m.controls_alpha();

        if has_lyrics && controls_alpha < 1.0 {
            c.save().unwrap();
            rounded_rect_path(c, left_edge, button_width, radius, bot, top);
            c.clip();
            c.push_group();
            draw_media_lyrics(
                c,
                style,
                text_color,
                &current_line,
                &m.prev_lyric,
                m.lyric_progress(),
                left_edge,
                button_width,
                bot,
                top,
            );
            c.pop_group_to_source().unwrap();
            c.paint_with_alpha(1.0 - controls_alpha).unwrap();
            c.restore().unwrap();
        }
        if controls_alpha > 0.0 {
            c.save().unwrap();
            rounded_rect_path(c, left_edge, button_width, radius, bot, top);
            c.clip();
            c.push_group();
            // Track text on the left, up to the control cluster.
            let (first, zone_w) = media_zone_geom(true, left_edge, button_width, icon_w);
            let text_left = left_edge + MEDIA_PAD;
            let text_width = (first - text_left - MEDIA_PAD).max(0.0);
            if text_width > 8.0 {
                draw_media_text(c, style, text_color, &info.title, &info.artist, text_left, text_width, bot, top);
            }
            draw_media_icons(c, m, &info, icon_w, first, zone_w, height, y_shift);
            c.pop_group_to_source().unwrap();
            c.paint_with_alpha(controls_alpha).unwrap();
            c.restore().unwrap();
        }
    } else {
        // Idle: three transport buttons spread across the full width.
        let (first, zone_w) = media_zone_geom(false, left_edge, button_width, icon_w);
        for (k, zone) in [MediaZone::Prev, MediaZone::PlayPause, MediaZone::Next].into_iter().enumerate() {
            let zleft = first + zone_w * k as f64;
            let inset = MEDIA_ICON_PAD.min(zone_w / 4.0);
            let fill = if m.pressed == Some(zone) {
                style.button_color_active
            } else {
                style.button_color
            };
            fill.set_source(c);
            rounded_rect_path(c, zleft + inset, zone_w - inset * 2.0, radius, bot, top);
            c.fill().unwrap();
        }
        draw_media_icons(c, m, &info, icon_w, first, zone_w, height, y_shift);
    }
}

/// Draw the three transport icons centered in their zones, play/pause tracking
/// the live status.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_media_icons(
    c: &Context,
    m: &MediaState,
    info: &media::MediaInfo,
    icon_w: f64,
    first_zone_left: f64,
    zone_w: f64,
    height: i32,
    y_shift: f64,
) {
    let play = if info.status == MediaStatus::Playing {
        m.pause_icon.as_ref()
    } else {
        m.play_icon.as_ref()
    };
    let icons = [m.prev_icon.as_ref(), play, m.next_icon.as_ref()];
    for (k, svg) in icons.into_iter().enumerate() {
        let Some(svg) = svg else { continue };
        let center = first_zone_left + zone_w * (k as f64 + 0.5);
        let x = (center - icon_w / 2.0).round();
        let y = (y_shift + (height as f64 - icon_w) / 2.0).round();
        svg.render(c, x, y, icon_w, icon_w);
    }
}

/// Draw the two-line title/artist block, left-aligned and ellipsized to fit.
/// The lines are sized to the band height (inside `MEDIA_TEXT_VPAD`), so they
/// stay clear of the top/bottom edges regardless of the configured FontSize.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_media_text(
    c: &Context,
    style: &style::Style,
    color: Color,
    title: &str,
    artist: &str,
    left: f64,
    width: f64,
    bot: f64,
    top: f64,
) {
    let avail = (top - bot - MEDIA_TEXT_VPAD * 2.0).max(1.0);
    let make = |text: &str, px: f64, bold: bool| {
        let layout = pangocairo::functions::create_layout(c);
        let mut font = style.font.clone();
        font.set_absolute_size(px * pango::SCALE as f64);
        font.set_weight(if bold { pango::Weight::Bold } else { pango::Weight::Normal });
        layout.set_font_description(Some(&font));
        layout.set_text(text);
        layout.set_ellipsize(pango::EllipsizeMode::End);
        layout.set_width((width * pango::SCALE as f64) as i32);
        layout
    };
    let title_layout = make(title, avail * 0.50, true);
    let artist_layout = make(artist, avail * 0.38, false);
    let (_, th) = title_layout.pixel_size();
    let (_, ah) = artist_layout.pixel_size();
    let total = th as f64 + MEDIA_TEXT_GAP + ah as f64;
    // Center the block in the band, but never above the top padding.
    let mut y = ((bot + top) / 2.0 - total / 2.0).max(bot + MEDIA_TEXT_VPAD);
    color.set_source(c);
    c.move_to(left, y);
    pangocairo::functions::show_layout(c, &title_layout);
    y += th as f64 + MEDIA_TEXT_GAP;
    Color { a: color.a * 0.8, ..color }.set_source(c);
    c.move_to(left, y);
    pangocairo::functions::show_layout(c, &artist_layout);
}

/// Draw the synced-lyric view: the currently-active line, centered and sized up
/// to fill the panel width. As the line advances it slides upward -- the
/// outgoing (`prev`) line moving up and out while the current one rises from
/// below into the centre -- with `progress` 0 (just changed) to 1 (settled).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_media_lyrics(
    c: &Context,
    style: &style::Style,
    color: Color,
    line: &str,
    prev: &str,
    progress: f64,
    left_edge: f64,
    button_width: f64,
    bot: f64,
    top: f64,
) {
    let band = top - bot;
    let mid_y = (bot + top) / 2.0;
    let left = left_edge + MEDIA_PAD;
    let width = (button_width - MEDIA_PAD * 2.0).max(1.0);
    // Smoothstep the travel; the lines move up by a full band height.
    let p = progress * progress * (3.0 - 2.0 * progress);
    let step = band;

    // Lay a line out at a font size that fills the band height, shrunk so a long
    // line fits the width (down to a floor, after which it ellipsizes).
    let draw = |text: &str, center_y: f64, alpha: f64| {
        if text.is_empty() || alpha <= 0.0 {
            return;
        }
        let layout = pangocairo::functions::create_layout(c);
        let mut font = style.font.clone();
        layout.set_text(text);
        let max_px = (band * 0.62).max(1.0);
        let min_px = band * 0.34;
        font.set_absolute_size(max_px * pango::SCALE as f64);
        layout.set_font_description(Some(&font));
        let (natural_w, _) = layout.pixel_size();
        let mut px = max_px;
        if natural_w as f64 > width {
            // Shrink to fit the width. Glyph advance isn't perfectly linear in
            // font size (hinting/rounding), so a single linear estimate can
            // still overflow and ellipsize -- refine by re-measuring until it
            // fits (or bottoms out at the floor, after which it ellipsizes).
            px = (max_px * width / natural_w as f64).max(min_px);
            for _ in 0..3 {
                font.set_absolute_size(px * pango::SCALE as f64);
                layout.set_font_description(Some(&font));
                let (w, _) = layout.pixel_size();
                if w as f64 <= width || px <= min_px {
                    break;
                }
                px = (px * width / w as f64).max(min_px);
            }
        }
        font.set_absolute_size(px * pango::SCALE as f64);
        layout.set_font_description(Some(&font));
        layout.set_ellipsize(pango::EllipsizeMode::End);
        layout.set_width((width * pango::SCALE as f64) as i32);
        layout.set_alignment(pango::Alignment::Center);
        let (_, h) = layout.pixel_size();
        Color {
            a: color.a * alpha,
            ..color
        }
        .set_source(c);
        c.move_to(left, center_y - h as f64 / 2.0);
        pangocairo::functions::show_layout(c, &layout);
    };

    if progress < 1.0 {
        // Outgoing line rising up and out, fading.
        draw(prev, mid_y - p * step, 1.0 - p);
        // Current line rising from below into the centre, fading in.
        draw(line, mid_y + (1.0 - p) * step, p);
    } else {
        draw(line, mid_y, 1.0);
    }
}
