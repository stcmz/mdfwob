//! Candlestick chart rendering.
//!
//! Turns a `[Bar]` series into a pixel image and emits it either as a Sixel escape sequence for a
//! terminal (the default) or as a PNG file. The renderer is deliberately dependency-light: it
//! rasterizes into an 8-bit **indexed** buffer over a fixed six-color palette, so the Sixel encoder
//! needs no quantization and the PNG is written as an indexed-color image.
//!
//! Layout mirrors a classic OHLC candlestick: a left price gutter with horizontal grid lines, one
//! candle per bar (high-low wick plus an open-close body, green when up and red when down), an
//! optional SMA overlay, month grid lines with labels along the bottom, and a title. All element
//! sizes scale with the canvas so a 4K image looks proportional to a 1080p one.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use font8x8::legacy::BASIC_LEGACY;
use jiff::{Timestamp, tz::TimeZone};

use crate::analysis::model::Bar;

/// Palette indices. The Sixel encoder and PNG writer share this exact table, so what a terminal
/// shows and what a file stores are pixel-for-pixel identical.
const BG: u8 = 0;
const GRID: u8 = 1;
const TEXT: u8 = 2;
const UP: u8 = 3;
const DOWN: u8 = 4;
const SMA: u8 = 5;

/// RGB for each palette index (0..=255 per channel).
const PALETTE: [(u8, u8, u8); 6] = [
    (18, 18, 24),    // 0 background
    (44, 46, 56),    // 1 grid
    (170, 174, 190), // 2 text
    (38, 194, 129),  // 3 up (green)
    (235, 77, 75),   // 4 down (red)
    (90, 160, 255),  // 5 sma (blue)
];

/// Default chart width in pixels.
pub const DEFAULT_WIDTH: u32 = 1920;
/// Default chart height in pixels.
pub const DEFAULT_HEIGHT: u32 = 1080;

/// Rendering options for [`render`].
#[derive(Debug, Clone)]
pub struct PlotOptions {
    pub width: u32,
    pub height: u32,
    /// Simple-moving-average window over closes, drawn as an overlay. `0` disables it.
    pub sma_period: usize,
    /// Title drawn at the top-left (e.g. `"AAPL 1d"`).
    pub title: String,
    /// Timezone the x-axis calendar boundaries and clock labels are expressed in. This should match
    /// the session tz the bars were aggregated against, so day/month/year ticks land on the same
    /// local boundaries the buckets were anchored to.
    pub tz: TimeZone,
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            sma_period: 20,
            title: String::new(),
            tz: TimeZone::UTC,
        }
    }
}

/// An 8-bit indexed image over [`PALETTE`], one byte (palette index) per pixel, row-major.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32, fill: u8) -> Self {
        Self {
            width,
            height,
            px: vec![fill; (width as usize) * (height as usize)],
        }
    }

    /// Fills the axis-aligned rectangle `[x, x+w) x [y, y+h)` (clipped to the canvas).
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u8) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i32);
        let y1 = (y + h).min(self.height as i32);
        for yy in y0..y1 {
            let row = (yy as usize) * (self.width as usize);
            for xx in x0..x1 {
                self.px[row + xx as usize] = color;
            }
        }
    }

    /// Stamps a `t`-by-`t` square centered on `(x, y)` — the brush used to give lines thickness.
    fn stamp(&mut self, x: i32, y: i32, t: i32, color: u8) {
        let half = t / 2;
        self.fill_rect(x - half, y - half, t.max(1), t.max(1), color);
    }

    /// Draws a `t`-thick line between two points via Bresenham with a square brush.
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, t: i32, color: u8) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.stamp(x, y, t, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Draws `text` with the top-left at `(x, y)`, each 8x8 glyph scaled by `scale`. Returns the
    /// advance width in pixels so callers can right-align.
    fn text(&mut self, x: i32, y: i32, text: &str, color: u8, scale: i32) {
        let s = scale.max(1);
        let mut cx = x;
        for ch in text.chars() {
            let code = ch as usize;
            if code < 128 {
                let glyph = BASIC_LEGACY[code];
                for (row, bits) in glyph.iter().enumerate() {
                    for col in 0..8 {
                        if bits & (1 << col) != 0 {
                            self.fill_rect(cx + col * s, y + row as i32 * s, s, s, color);
                        }
                    }
                }
            }
            cx += 8 * s;
        }
    }

    /// Consumes the canvas into an ARGB-free indexed byte buffer (length `width * height`).
    pub fn into_indexed(self) -> Vec<u8> {
        self.px
    }

    /// Encodes the canvas as a Sixel escape sequence (DCS `q` ... ST). The image renders inline in
    /// terminals that support Sixel (recent Windows Terminal, xterm, WezTerm, iTerm2, ...).
    pub fn to_sixel(&self) -> String {
        encode_sixel(&self.px, self.width, self.height)
    }

    /// Writes the canvas to `path` as an indexed-color PNG.
    pub fn write_png(&self, path: &Path) -> Result<()> {
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        let mut palette = Vec::with_capacity(PALETTE.len() * 3);
        for (r, g, b) in PALETTE {
            palette.extend_from_slice(&[r, g, b]);
        }
        encoder.set_palette(palette);
        let mut writer = encoder
            .write_header()
            .with_context(|| format!("failed to write PNG header to {}", path.display()))?;
        writer
            .write_image_data(&self.px)
            .with_context(|| format!("failed to write PNG data to {}", path.display()))?;
        Ok(())
    }
}

/// Renders `bars` into a candlestick [`Canvas`]. Returns an all-background canvas when `bars` is
/// empty so callers still get a valid image of the requested size.
pub fn render(bars: &[Bar], opts: &PlotOptions) -> Canvas {
    let w = opts.width.max(16);
    let h = opts.height.max(16);
    let mut canvas = Canvas::new(w, h, BG);
    let n = bars.len();
    if n == 0 {
        return canvas;
    }

    // Element scale: 1.0 at 1920x1080, growing with the smaller of the two axis ratios so a 4K
    // canvas keeps the same proportions.
    let ds = (h as f32 / DEFAULT_HEIGHT as f32).min(w as f32 / DEFAULT_WIDTH as f32);
    let text_scale = ((1.6 * ds).round() as i32).max(1);
    let char_w = 8 * text_scale;

    // Price extent with a small headroom pad.
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for bar in bars {
        min = min.min(bar.low);
        max = max.max(bar.high);
    }
    if !min.is_finite() || !max.is_finite() {
        return canvas;
    }
    let pad = ((max - min) * 0.04).max(f64::MIN_POSITIVE);
    min -= pad;
    max += pad;
    let range = (max - min).max(1e-9);

    // Price gutter labels, computed up front so the left margin can be sized to the widest one
    // (otherwise long prices like "1,234.56" overflow a fixed gutter and get clipped).
    let levels = 6;
    let labels: Vec<String> = (0..=levels)
        .map(|k| format!("{:.2}", min + range * k as f64 / levels as f64))
        .collect();
    let label_gap = (8.0 * ds) as i32;
    let max_label_w = labels
        .iter()
        .map(|l| l.chars().count() as i32 * char_w)
        .max()
        .unwrap_or(0);

    let left = max_label_w + label_gap * 2;
    let right = (14.0 * ds) as i32;
    let top = (22.0 * ds) as i32;
    let bottom = (34.0 * ds) as i32;
    let plot_w = (w as i32 - left - right).max(1);
    let plot_h = (h as i32 - top - bottom).max(1);

    let y_of = |p: f64| -> i32 { (top as f64 + (max - p) / range * plot_h as f64).round() as i32 };
    let x_of =
        |i: usize| -> i32 { left + ((i as f64 + 0.5) * (plot_w as f64 / n as f64)).round() as i32 };

    let grid_t = ((ds).round() as i32).max(1);
    let pen_t = grid_t;
    let sma_t = ((2.0 * ds).round() as i32).max(1);

    // Time-axis boundaries (positions only), chosen to suit the visible span (years for a
    // multi-year view down to minutes for an intraday one), expressed in the configured timezone.
    let (gran, time_ticks) = time_axis_ticks(bars, &opts.tz);

    // Resolve label geometry and drop collisions so labels never overprint each other. Two labels
    // can collide when the first bar sits mid-period (a leading partial tick right next to the
    // first real boundary) or when a coarse label is simply wider than the tick spacing.
    let min_gap = char_w; // keep at least one blank character between labels
    let all: Vec<usize> = (0..time_ticks.len()).collect();
    let boxes: Vec<(i32, i32)> = label_ticks(gran, &time_ticks, &all)
        .iter()
        .map(|(i, label)| {
            let tw = label.chars().count() as i32 * char_w;
            let lx = x_of(*i).min(w as i32 - tw - label_gap).max(0);
            (lx, tw)
        })
        .collect();
    // Label the kept ticks using the previous *kept* tick as context, so whichever tick becomes
    // first-visible after a collision drop still carries the year (the dropped leading tick took
    // its year with it otherwise). Recompute geometry from the final label widths.
    let keep = select_nonoverlapping(&boxes, min_gap);
    let placed: Vec<(usize, i32, String)> = label_ticks(gran, &time_ticks, &keep)
        .into_iter()
        .map(|(i, label)| {
            let tw = label.chars().count() as i32 * char_w;
            let lx = x_of(i).min(w as i32 - tw - label_gap).max(0);
            (i, lx, label)
        })
        .collect();

    // Grid first (both axes), so the candles and overlays paint on top of it.
    // Horizontal price grid + right-aligned labels in the gutter.
    for (k, label) in labels.iter().enumerate() {
        let p = min + range * k as f64 / levels as f64;
        let y = y_of(p);
        canvas.fill_rect(left, y, plot_w, grid_t, GRID);
        let tw = label.chars().count() as i32 * char_w;
        canvas.text(
            left - label_gap - tw,
            y - 4 * text_scale,
            label,
            TEXT,
            text_scale,
        );
    }
    // Vertical time-axis grid lines, at the same boundaries as the (kept) labels.
    for (i, _, _) in &placed {
        canvas.fill_rect(x_of(*i), top, grid_t, plot_h, GRID);
    }

    // Candles: high-low wick, then the open-close body.
    let body_w = ((plot_w as f64 / n as f64) * 0.62).max(1.5 * ds as f64) as i32;
    let min_body = (1.5 * ds).max(1.0) as i32;
    for (i, bar) in bars.iter().enumerate() {
        let x = x_of(i);
        let up = bar.close >= bar.open;
        let color = if up { UP } else { DOWN };
        // Wick.
        let yh = y_of(bar.high);
        let yl = y_of(bar.low);
        canvas.fill_rect(x - pen_t / 2, yh, pen_t.max(1), (yl - yh).max(1), color);
        // Body.
        let yo = y_of(bar.open);
        let yc = y_of(bar.close);
        let bt = yo.min(yc);
        let bh = (yo - yc).abs().max(min_body);
        canvas.fill_rect(x - body_w / 2, bt, body_w.max(1), bh, color);
    }

    // SMA(close) overlay.
    if opts.sma_period >= 2 && opts.sma_period <= n {
        let period = opts.sma_period;
        let mut sum = 0.0;
        let mut prev: Option<(i32, i32)> = None;
        for (i, bar) in bars.iter().enumerate() {
            sum += bar.close;
            if i >= period {
                sum -= bars[i - period].close;
            }
            if i + 1 >= period {
                let avg = sum / period as f64;
                let pt = (x_of(i), y_of(avg));
                if let Some(prev) = prev {
                    canvas.line(prev.0, prev.1, pt.0, pt.1, sma_t, SMA);
                }
                prev = Some(pt);
            }
        }
    }

    // Time-axis labels along the bottom (on top of the grid and candles), using the collision-free
    // set and pixel positions resolved above.
    let label_y = h as i32 - bottom + (6.0 * ds) as i32;
    for (_, lx, label) in &placed {
        canvas.text(*lx, label_y, label, TEXT, text_scale);
    }

    // Title.
    if !opts.title.is_empty() {
        canvas.text(left, (3.0 * ds) as i32, &opts.title, TEXT, text_scale);
    }

    canvas
}

/// The granularity of a time-axis tick, chosen from the visible span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gran {
    /// Sub-day step of `n` seconds (minutes/hours).
    Sec(i64),
    Day,
    Week,
    /// A run of `n` calendar months (1 = monthly, 3 = quarterly).
    Month(i64),
    /// A run of `n` calendar years.
    Year(i64),
}

impl Gran {
    /// The bucket index a timestamp falls in; a tick is emitted whenever this changes between
    /// consecutive bars.
    fn bucket(self, t: i64) -> i64 {
        match self {
            Gran::Sec(step) => t.div_euclid(step),
            Gran::Day => t.div_euclid(86_400),
            // Week buckets aligned to Monday (epoch day 0 = Thursday, so shift by 3).
            Gran::Week => (t.div_euclid(86_400) + 3).div_euclid(7),
            Gran::Month(k) => {
                let (y, m, _) = ymd(t);
                (i64::from(y) * 12 + i64::from(m - 1)).div_euclid(k)
            }
            Gran::Year(k) => i64::from(ymd(t).0).div_euclid(k),
        }
    }

    /// The label for a tick at (local) time `t`. `prev` is the previous tick's local time, used so a
    /// sub-day axis shows a date when the day changes (a clock time otherwise) and a monthly axis
    /// shows the year only on the first tick of each new year (the leading tick counts as a change).
    fn label(self, t: i64, prev: Option<i64>) -> String {
        match self {
            Gran::Sec(_) => {
                let day = t.div_euclid(86_400);
                let prev_day = prev.map(|p| p.div_euclid(86_400));
                if prev_day != Some(day) {
                    strf(t, "%b %d")
                } else {
                    strf(t, "%H:%M")
                }
            }
            Gran::Day | Gran::Week => strf(t, "%b %d"),
            Gran::Month(_) => {
                let year = ymd(t).0;
                let prev_year = prev.map(|p| ymd(p).0);
                if prev_year == Some(year) {
                    strf(t, "%b")
                } else {
                    strf(t, "%b %Y")
                }
            }
            Gran::Year(_) => strf(t, "%Y"),
        }
    }
}

/// Picks time-axis ticks that suit the visible span: years for a multi-year view, months for a
/// yearly one, days/hours/minutes as the window shrinks. Returns `(bar_index, label)` pairs at each
/// boundary of the chosen granularity, targeting a readable number of labels across the width.
///
/// Calendar boundaries and clock labels are computed in `tz` so they line up with the local
/// day/month/year the bars were aggregated against (each timestamp is shifted by its `tz` offset,
/// then treated as civil time).
fn time_axis_ticks(bars: &[Bar], tz: &TimeZone) -> (Gran, Vec<(usize, i64)>) {
    let n = bars.len();
    if n == 0 {
        return (Gran::Day, Vec::new());
    }
    let span = (i64::from(bars[n - 1].time) - i64::from(bars[0].time)).max(1);
    // Cap the label count; the finest ladder step that stays under it wins, which keeps roughly
    // 7..14 labels (adjacent steps differ by ~2-4x).
    const MAX_TICKS: i64 = 14;
    // (approximate seconds per step, granularity), ascending.
    const LADDER: [(i64, Gran); 15] = [
        (300, Gran::Sec(300)),         // 5m
        (900, Gran::Sec(900)),         // 15m
        (1_800, Gran::Sec(1_800)),     // 30m
        (3_600, Gran::Sec(3_600)),     // 1h
        (10_800, Gran::Sec(10_800)),   // 3h
        (21_600, Gran::Sec(21_600)),   // 6h
        (43_200, Gran::Sec(43_200)),   // 12h
        (86_400, Gran::Day),           // 1d
        (604_800, Gran::Week),         // 1w
        (2_629_800, Gran::Month(1)),   // 1mo
        (7_889_400, Gran::Month(3)),   // 1 quarter
        (31_557_600, Gran::Year(1)),   // 1y
        (63_115_200, Gran::Year(2)),   // 2y
        (157_788_000, Gran::Year(5)),  // 5y
        (315_576_000, Gran::Year(10)), // 10y
    ];
    let gran = LADDER
        .iter()
        .find(|(secs, _)| span / secs <= MAX_TICKS)
        .map(|&(_, g)| g)
        .unwrap_or(Gran::Sec(60));

    // Emit a tick at each granularity-bucket boundary, carrying the bar index and its local time.
    // Labeling is deferred to [`label_ticks`] so it can be done against the surviving (kept) ticks
    // after collision resolution, keeping the year on whichever tick ends up first-visible.
    let mut ticks = Vec::new();
    let mut prev_bucket: Option<i64> = None;
    for (i, bar) in bars.iter().enumerate() {
        // Shift into local civil time so all calendar/clock math is tz-correct.
        let local = local_epoch(i64::from(bar.time), tz);
        let bucket = gran.bucket(local);
        if prev_bucket != Some(bucket) {
            ticks.push((i, local));
            prev_bucket = Some(bucket);
        }
    }
    (gran, ticks)
}

/// Labels the ticks at indices `keep` (into `ticks`, each `(bar_index, local_time)`), using the
/// previous *kept* tick as context. So the first kept tick — and any kept tick that starts a new
/// year or a new day — shows the fuller form (year / date), while runs within the same year or day
/// stay compact. Returns `(bar_index, label)` for each kept tick, in order.
fn label_ticks(gran: Gran, ticks: &[(usize, i64)], keep: &[usize]) -> Vec<(usize, String)> {
    let mut out = Vec::with_capacity(keep.len());
    let mut prev: Option<i64> = None;
    for &k in keep {
        let (bar, local) = ticks[k];
        out.push((bar, gran.label(local, prev)));
        prev = Some(local);
    }
    out
}

/// Shifts a UTC epoch second into local civil seconds for `tz` (i.e. `t + utc_offset(t)`), so the
/// UTC-based [`ymd`]/[`strf`] helpers yield `tz`-local calendar fields and clock times. The offset
/// is resolved per instant, so DST transitions are handled.
fn local_epoch(t: i64, tz: &TimeZone) -> i64 {
    match Timestamp::from_second(t) {
        Ok(ts) => t + i64::from(tz.to_offset(ts).seconds()),
        Err(_) => t,
    }
}

/// Selects which time-axis labels to draw so none overprint another. `boxes` are the labels'
/// `(left_x, width)` in draw order (left to right); `min_gap` is the minimum blank space required
/// between two labels. Returns the kept indices.
///
/// A leading label that would collide with the second one is dropped in favor of the second: the
/// first bar of a window often sits mid-period (a short "leading partial" tick sitting right next
/// to the first real period boundary), and the boundary is the more useful of the two. After that
/// it is a greedy left-to-right sweep that keeps a label only once it clears the previous one.
fn select_nonoverlapping(boxes: &[(i32, i32)], min_gap: i32) -> Vec<usize> {
    let start = usize::from(boxes.len() >= 2 && boxes[0].0 + boxes[0].1 + min_gap > boxes[1].0);
    let mut kept = Vec::new();
    let mut last_right = i32::MIN;
    for (i, &(lx, tw)) in boxes.iter().enumerate().skip(start) {
        if lx >= last_right.saturating_add(min_gap) {
            last_right = lx + tw;
            kept.push(i);
        }
    }
    kept
}

/// Decomposes a UTC epoch second into `(year, month, day)`; falls back to the epoch on overflow.
fn ymd(t: i64) -> (i16, i8, i8) {
    match Timestamp::from_second(t) {
        Ok(ts) => {
            let z = ts.to_zoned(TimeZone::UTC);
            (z.year(), z.month(), z.day())
        }
        Err(_) => (1970, 1, 1),
    }
}

/// Formats a UTC epoch second with a jiff `strftime` pattern.
fn strf(t: i64, fmt: &str) -> String {
    match Timestamp::from_second(t) {
        Ok(ts) => ts.to_zoned(TimeZone::UTC).strftime(fmt).to_string(),
        Err(_) => String::new(),
    }
}

/// Encodes an indexed pixel buffer over [`PALETTE`] as a Sixel escape sequence.
///
/// Sixel packs six vertical pixels per character; the image is drawn as `ceil(h/6)` horizontal
/// bands, once per palette color, with run-length encoding of identical six-pixel columns.
fn encode_sixel(px: &[u8], width: u32, height: u32) -> String {
    let w = width as usize;
    let h = height as usize;
    let nc = PALETTE.len();
    let mut out = String::new();
    // DCS: enter Sixel, aspect 1:1, declare raster attributes.
    out.push('\u{1b}');
    out.push_str("P0;0;0q");
    out.push_str(&format!("\"1;1;{width};{height}"));
    // Color registers, channels scaled to 0..100.
    for (i, (r, g, b)) in PALETTE.iter().enumerate() {
        let r = (f64::from(*r) / 255.0 * 100.0).round() as i32;
        let g = (f64::from(*g) / 255.0 * 100.0).round() as i32;
        let b = (f64::from(*b) / 255.0 * 100.0).round() as i32;
        out.push_str(&format!("#{i};2;{r};{g};{b}"));
    }

    let bands = h.div_ceil(6);
    // For each color, the six-pixel bit column at every x in the current band.
    let mut bcols = vec![0u8; nc * w];
    let mut seg = String::new();
    for by in 0..bands {
        for v in bcols.iter_mut() {
            *v = 0;
        }
        let y0 = by * 6;
        for ry in 0..6 {
            let y = y0 + ry;
            if y >= h {
                break;
            }
            let row = y * w;
            let bit = 1u8 << ry;
            for x in 0..w {
                let ci = px[row + x] as usize;
                bcols[ci * w + x] |= bit;
            }
        }
        for ci in 0..nc {
            let off = ci * w;
            let mut any = false;
            seg.clear();
            let mut run_ch: i32 = -1;
            let mut run_len = 0usize;
            for x in 0..w {
                let bits = bcols[off + x];
                if bits != 0 {
                    any = true;
                }
                let ch = 63 + bits as i32;
                if ch == run_ch {
                    run_len += 1;
                } else {
                    flush_run(&mut seg, run_ch, run_len);
                    run_ch = ch;
                    run_len = 1;
                }
            }
            flush_run(&mut seg, run_ch, run_len);
            if any {
                out.push('#');
                out.push_str(&ci.to_string());
                out.push_str(&seg);
                out.push('$');
            }
        }
        out.push('-');
    }
    out.push('\u{1b}');
    out.push('\\');
    out
}

/// Appends a run of `len` copies of Sixel char `ch`, using `!count` RLE for runs of four or more.
fn flush_run(seg: &mut String, ch: i32, len: usize) {
    if ch < 0 || len == 0 {
        return;
    }
    let c = (ch as u8) as char;
    if len >= 4 {
        seg.push('!');
        seg.push_str(&len.to_string());
        seg.push(c);
    } else {
        for _ in 0..len {
            seg.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(time: u32, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            time,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 0,
            vwap: f64::NAN,
            trades: 0,
        }
    }

    fn sample() -> Vec<Bar> {
        // A short deterministic zig-zag across two months.
        let mut bars = Vec::new();
        let mut price: f64 = 100.0;
        for i in 0..40u32 {
            let o = price;
            let c = o + if i % 2 == 0 { 3.0 } else { -2.0 };
            let hi = o.max(c) + 1.0;
            let lo = o.min(c) - 1.0;
            price = c;
            bars.push(bar(1_735_700_000 + i * 86_400, o, hi, lo, c));
        }
        bars
    }

    #[test]
    fn render_produces_requested_dimensions() {
        let opts = PlotOptions {
            width: 640,
            height: 360,
            sma_period: 5,
            title: "TEST 1d".into(),
            tz: jiff::tz::TimeZone::UTC,
        };
        let canvas = render(&sample(), &opts);
        assert_eq!(canvas.width, 640);
        assert_eq!(canvas.height, 360);
        assert_eq!(canvas.px.len(), 640 * 360);
    }

    #[test]
    fn render_draws_candles_and_overlays() {
        let opts = PlotOptions {
            width: 640,
            height: 360,
            sma_period: 5,
            title: "TEST 1d".into(),
            tz: jiff::tz::TimeZone::UTC,
        };
        let canvas = render(&sample(), &opts);
        // Every non-background palette color should appear somewhere.
        for color in [GRID, TEXT, UP, DOWN, SMA] {
            assert!(
                canvas.px.contains(&color),
                "expected palette color {color} to be drawn"
            );
        }
    }

    #[test]
    fn wide_price_labels_are_not_clipped_at_the_left_edge() {
        // Prices in the thousands produce wide gutter labels ("1176.80".."1263.20"); the left
        // margin must grow to fit them so nothing is clipped against the canvas border.
        let bars: Vec<Bar> = (0..10)
            .map(|i| bar(1_735_700_000 + i * 86_400, 1200.0, 1260.0, 1180.0, 1230.0))
            .collect();
        let opts = PlotOptions {
            width: 640,
            height: 360,
            sma_period: 0,
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
        };
        let canvas = render(&bars, &opts);
        // The far-left column must be entirely background: labels keep a gap and never clip.
        for y in 0..canvas.height {
            let idx = (y as usize) * canvas.width as usize;
            assert_eq!(
                canvas.px[idx], BG,
                "content clipped to the left edge at row {y}"
            );
        }
        // Labels were actually drawn in the gutter.
        assert!(canvas.px.contains(&TEXT));
    }

    #[test]
    fn right_edge_stays_clear_of_candles_and_labels() {
        // Neither the last candle nor the last month label may spill past the right border.
        let canvas = render(
            &sample(),
            &PlotOptions {
                width: 800,
                height: 400,
                sma_period: 0,
                title: String::new(),
                tz: jiff::tz::TimeZone::UTC,
            },
        );
        let w = canvas.width as usize;
        let h = canvas.height as usize;
        for y in 0..h {
            assert_eq!(
                canvas.px[y * w + (w - 1)],
                BG,
                "content clipped at right edge row {y}"
            );
        }
    }

    #[test]
    fn candles_are_drawn_over_the_month_grid_lines() {
        let canvas = render(
            &sample(),
            &PlotOptions {
                width: 800,
                height: 400,
                sma_period: 0,
                title: String::new(),
                tz: jiff::tz::TimeZone::UTC,
            },
        );
        let w = canvas.width as usize;
        let h = canvas.height as usize;
        // A vertical month grid line is a column that is GRID for a large fraction of its height
        // (ordinary columns only cross the ~6 horizontal grid lines). At each such column a candle
        // must still show through, proving candles paint on top of the grid rather than under it.
        let mut found_gridline = false;
        for x in 0..w {
            let grid_rows = (0..h).filter(|&y| canvas.px[y * w + x] == GRID).count();
            if grid_rows > h / 3 {
                found_gridline = true;
                let has_candle = (0..h).any(|y| matches!(canvas.px[y * w + x], UP | DOWN));
                assert!(
                    has_candle,
                    "month grid line at column {x} overwrote the candle"
                );
            }
        }
        assert!(
            found_gridline,
            "expected at least one vertical month grid line"
        );
    }

    fn epoch(y: i16, m: i8, d: i8) -> u32 {
        jiff::civil::date(y, m, d)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
            .as_second() as u32
    }

    fn flat_bar(time: u32) -> Bar {
        bar(time, 100.0, 101.0, 99.0, 100.5)
    }

    /// Full label pipeline with every tick kept (no collisions) — the common case for these tests.
    fn labeled(bars: &[Bar], tz: &jiff::tz::TimeZone) -> Vec<(usize, String)> {
        let (gran, ticks) = time_axis_ticks(bars, tz);
        let all: Vec<usize> = (0..ticks.len()).collect();
        label_ticks(gran, &ticks, &all)
    }

    #[test]
    fn time_axis_uses_year_labels_over_a_multi_year_span() {
        // One bar on the first of each month across six calendar years.
        let mut bars = Vec::new();
        for y in 2020..2026i16 {
            for m in 1..=12i8 {
                bars.push(flat_bar(epoch(y, m, 1)));
            }
        }
        let ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        assert!(ticks.len() <= 14, "too many labels: {}", ticks.len());
        let labels: Vec<&str> = ticks.iter().map(|(_, l)| l.as_str()).collect();
        // A multi-year span is labeled by year.
        assert!(labels.contains(&"2020"), "labels = {labels:?}");
        assert!(labels.contains(&"2025"), "labels = {labels:?}");
        assert!(
            labels
                .iter()
                .all(|l| l.len() == 4 && l.chars().all(|c| c.is_ascii_digit())),
            "expected pure year labels, got {labels:?}"
        );
    }

    #[test]
    fn time_axis_shows_a_year_anchor_over_a_single_year_of_daily_bars() {
        // Daily bars for one calendar year -> monthly ticks; the first month carries the year.
        let base = epoch(2026, 1, 1);
        let bars: Vec<Bar> = (0..365u32).map(|i| flat_bar(base + i * 86_400)).collect();
        let ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        let labels: Vec<&str> = ticks.iter().map(|(_, l)| l.as_str()).collect();
        assert!(ticks.len() <= 14, "too many labels: {}", ticks.len());
        assert!(
            labels.contains(&"Jan 2026"),
            "expected a year anchor on the first month, got {labels:?}"
        );
        assert!(
            labels.contains(&"Feb"),
            "expected bare month labels within the year, got {labels:?}"
        );
    }

    #[test]
    fn monthly_axis_shows_the_year_only_on_the_first_month_of_each_year() {
        // Range Feb 2026 .. Jan 2027 (one bar per month): only the first visible month of a year
        // carries the year -> "Feb 2026" then bare months, then "Jan 2027".
        let mut bars = Vec::new();
        for (y, m) in [
            (2026i16, 2i8),
            (2026, 3),
            (2026, 4),
            (2026, 5),
            (2026, 6),
            (2026, 7),
            (2026, 8),
            (2026, 9),
            (2026, 10),
            (2026, 11),
            (2026, 12),
            (2027, 1),
        ] {
            bars.push(flat_bar(epoch(y, m, 1)));
        }
        let ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        let labels: Vec<&str> = ticks.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels.first(), Some(&"Feb 2026"), "labels = {labels:?}");
        assert!(labels.contains(&"Jan 2027"), "labels = {labels:?}");
        // No other label carries a year (they are bare months like "Mar", "Apr", ...).
        let with_year: Vec<&&str> = labels.iter().filter(|l| l.contains(' ')).collect();
        assert_eq!(
            with_year,
            vec![&"Feb 2026", &"Jan 2027"],
            "only the first month of each year should show the year, got {labels:?}"
        );
    }

    #[test]
    fn dropped_leading_tick_keeps_the_year_on_the_first_visible_label() {
        // Monthly ticks all within 2026 (Feb..Dec). When the leading Feb tick is dropped (as
        // happens when it collides with the March boundary), the surviving first-visible label must
        // still carry the year -> "Mar 2026", not a bare "Mar".
        let bars: Vec<Bar> = (2..=12i8).map(|m| flat_bar(epoch(2026, m, 1))).collect();
        let (gran, ticks) = time_axis_ticks(&bars, &jiff::tz::TimeZone::UTC);
        assert!(
            ticks.len() >= 3,
            "expected monthly ticks, got {}",
            ticks.len()
        );
        // Keep everything except the leading tick, exactly as a collision drop would.
        let keep: Vec<usize> = (1..ticks.len()).collect();
        let labels = label_ticks(gran, &ticks, &keep);
        assert_eq!(
            labels[0].1, "Mar 2026",
            "first visible label must carry the year after a leading drop"
        );
        // Without a drop, the year rides on the leading tick and March stays bare.
        let full = labeled(&bars, &jiff::tz::TimeZone::UTC);
        assert_eq!(full[0].1, "Feb 2026");
        assert_eq!(full[1].1, "Mar");
    }

    #[test]
    fn calendar_boundaries_follow_the_configured_timezone() {
        // 2026-01-01T02:00:00Z is still 2025-12-31 21:00 in New York. Daily bars starting there
        // should label the first tick "Dec 31" (NY) rather than "Jan 01" (UTC).
        let ny = jiff::tz::TimeZone::get("America/New_York").unwrap();
        let base = epoch(2026, 1, 1) + 2 * 3600; // 02:00 UTC
        let bars: Vec<Bar> = (0..20u32).map(|i| flat_bar(base + i * 86_400)).collect();
        let utc_ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        let ny_ticks = labeled(&bars, &ny);
        assert_eq!(utc_ticks[0].1, "Jan 01", "utc = {:?}", utc_ticks[0]);
        assert_eq!(ny_ticks[0].1, "Dec 31", "ny = {:?}", ny_ticks[0]);
    }

    #[test]
    fn time_axis_uses_clock_labels_for_an_intraday_span() {
        // 5-minute bars across a single RTH session -> intraday (HH:MM) ticks, first one dated.
        let base = epoch(2026, 7, 2) + 13 * 3600 + 30 * 60; // 13:30 UTC
        let bars: Vec<Bar> = (0..78u32).map(|i| flat_bar(base + i * 300)).collect();
        let ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        let labels: Vec<&str> = ticks.iter().map(|(_, l)| l.as_str()).collect();
        assert!(ticks.len() <= 14, "too many labels: {}", ticks.len());
        // Clock labels appear...
        assert!(
            labels.iter().any(|l| l.contains(':')),
            "expected HH:MM labels, got {labels:?}"
        );
        // ...and the first label is a date for context (no colon).
        assert!(
            !labels[0].contains(':'),
            "expected the first label to be a date, got {labels:?}"
        );
    }

    #[test]
    fn leading_partial_label_yields_to_the_first_boundary() {
        // A short leading tick at x=10 sits right next to the first real boundary at x=20; with a
        // min gap of 8 they collide, so the leading one is dropped in favor of the boundary.
        let boxes = [(10, 60), (20, 60), (400, 60), (800, 60)];
        let keep = select_nonoverlapping(&boxes, 8);
        assert_eq!(
            keep,
            vec![1, 2, 3],
            "leading partial should lose to the boundary"
        );
    }

    #[test]
    fn nonoverlapping_labels_are_all_kept() {
        let boxes = [(0, 40), (200, 40), (400, 40)];
        assert_eq!(select_nonoverlapping(&boxes, 8), vec![0, 1, 2]);
    }

    #[test]
    fn crowded_labels_are_thinned_left_to_right() {
        // Five equally wide labels packed closer than their width: keep every other one or so.
        let boxes = [(0, 50), (30, 50), (60, 50), (90, 50), (120, 50)];
        let keep = select_nonoverlapping(&boxes, 8);
        // No two kept labels overlap.
        for pair in keep.windows(2) {
            let (lx0, tw0) = boxes[pair[0]];
            let (lx1, _) = boxes[pair[1]];
            assert!(lx1 >= lx0 + tw0 + 8, "kept labels {pair:?} overlap");
        }
        assert!(keep.len() >= 2, "should keep more than one label");
    }

    #[test]
    fn weekly_bars_across_a_year_boundary_do_not_overlap_labels() {
        // Reproduces the reported case: weekly bars from late Dec 2024 through mid-2026. The
        // leading Dec-2024 partial tick must not collide with the Jan-2025 quarter boundary.
        let start = epoch(2024, 12, 30);
        let bars: Vec<Bar> = (0..78u32)
            .map(|i| flat_bar(start + i * 7 * 86_400))
            .collect();
        let opts = PlotOptions {
            width: 1280,
            height: 720,
            sma_period: 0,
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
        };
        // Recompute the label boxes the way render() does and assert the kept set is overlap-free.
        let ticks = labeled(&bars, &jiff::tz::TimeZone::UTC);
        let ds = (opts.height as f32 / DEFAULT_HEIGHT as f32)
            .min(opts.width as f32 / DEFAULT_WIDTH as f32);
        let char_w = ((1.6 * ds).round() as i32).max(1) * 8;
        let label_gap = (8.0 * ds) as i32;
        let x_of = |i: usize| -> i32 {
            // Approximate; only relative spacing matters for the overlap check.
            120 + ((i as f64 + 0.5) * ((opts.width as f64 - 140.0) / bars.len() as f64)).round()
                as i32
        };
        let boxes: Vec<(i32, i32)> = ticks
            .iter()
            .map(|(i, l)| {
                let tw = l.chars().count() as i32 * char_w;
                let lx = x_of(*i).min(opts.width as i32 - tw - label_gap).max(0);
                (lx, tw)
            })
            .collect();
        let keep = select_nonoverlapping(&boxes, char_w);
        for pair in keep.windows(2) {
            let (lx0, tw0) = boxes[pair[0]];
            let (lx1, _) = boxes[pair[1]];
            assert!(
                lx1 >= lx0 + tw0 + char_w,
                "labels overlap: {:?} vs {:?}",
                ticks[pair[0]],
                ticks[pair[1]]
            );
        }
        // And the render path itself must not panic on this data.
        let _ = render(&bars, &opts);
    }

    #[test]
    fn empty_series_yields_blank_canvas_of_requested_size() {
        let opts = PlotOptions {
            width: 320,
            height: 200,
            sma_period: 0,
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
        };
        let canvas = render(&[], &opts);
        assert_eq!(canvas.px.len(), 320 * 200);
        assert!(canvas.px.iter().all(|&p| p == BG));
    }

    #[test]
    fn sixel_is_well_formed() {
        let opts = PlotOptions {
            width: 120,
            height: 80,
            sma_period: 5,
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
        };
        let sixel = render(&sample(), &opts).to_sixel();
        assert!(
            sixel.starts_with("\u{1b}P0;0;0q"),
            "missing Sixel DCS intro"
        );
        assert!(sixel.ends_with("\u{1b}\\"), "missing Sixel ST terminator");
        assert!(sixel.contains("\"1;1;120;80"), "missing raster attributes");
        // Each palette register should be declared.
        for i in 0..PALETTE.len() {
            assert!(sixel.contains(&format!("#{i};2;")), "missing color reg {i}");
        }
    }

    #[test]
    fn png_round_trips_through_the_decoder() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mdfwob_plot_test_{}.png", std::process::id()));
        let opts = PlotOptions {
            width: 200,
            height: 120,
            sma_period: 5,
            title: "PNG".into(),
            tz: jiff::tz::TimeZone::UTC,
        };
        render(&sample(), &opts).write_png(&path).unwrap();

        let decoder = png::Decoder::new(File::open(&path).unwrap());
        let mut reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!(info.width, 200);
        assert_eq!(info.height, 120);
        assert_eq!(info.color_type, png::ColorType::Indexed);
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        std::fs::remove_file(&path).ok();
    }
}
