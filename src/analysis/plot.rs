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
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            sma_period: 20,
            title: String::new(),
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

    // Month boundaries: the first bar of each new month. Used for the vertical grid lines (drawn
    // under the candles) and the bottom axis labels (drawn on top, after the candles).
    let month_ticks: Vec<(usize, String)> = {
        let mut ticks = Vec::new();
        let mut last = String::new();
        for (i, bar) in bars.iter().enumerate() {
            let month = month_label(bar.time);
            if month != last {
                ticks.push((i, month.clone()));
                last = month;
            }
        }
        ticks
    };

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
    // Vertical month grid lines.
    for (i, _) in &month_ticks {
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

    // Month labels along the bottom (on top of the grid and candles). Clamp each label's x so the
    // last one does not run off the right edge of the canvas.
    let label_y = h as i32 - bottom + (6.0 * ds) as i32;
    for (i, month) in &month_ticks {
        let tw = month.chars().count() as i32 * char_w;
        let lx = x_of(*i).min(w as i32 - tw - label_gap).max(0);
        canvas.text(lx, label_y, month, TEXT, text_scale);
    }

    // Title.
    if !opts.title.is_empty() {
        canvas.text(left, (3.0 * ds) as i32, &opts.title, TEXT, text_scale);
    }

    canvas
}

/// Formats a UTC epoch second as a three-letter month abbreviation (`Jan`..`Dec`).
fn month_label(epoch: u32) -> String {
    match Timestamp::from_second(i64::from(epoch)) {
        Ok(ts) => ts.to_zoned(TimeZone::UTC).strftime("%b").to_string(),
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

    #[test]
    fn empty_series_yields_blank_canvas_of_requested_size() {
        let opts = PlotOptions {
            width: 320,
            height: 200,
            sma_period: 0,
            title: String::new(),
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
