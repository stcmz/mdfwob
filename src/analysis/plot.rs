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
/// First series/overlay color; indicator lines cycle through [`SERIES_COUNT`] of them.
const SERIES0: u8 = 5;
const SERIES_COUNT: u8 = 6;

/// RGB for each palette index (0..=255 per channel).
const PALETTE: [(u8, u8, u8); 11] = [
    (18, 18, 24),    // 0 background
    (44, 46, 56),    // 1 grid
    (170, 174, 190), // 2 text
    (38, 194, 129),  // 3 up (green)
    (235, 77, 75),   // 4 down (red)
    (90, 160, 255),  // 5 series: blue
    (240, 150, 60),  // 6 series: orange
    (185, 130, 245), // 7 series: purple
    (70, 200, 200),  // 8 series: teal
    (220, 205, 90),  // 9 series: yellow
    (240, 120, 180), // 10 series: pink
];

/// The palette index for the `i`-th indicator series, cycling through the series colors.
fn series_color(i: usize) -> u8 {
    SERIES0 + (i % SERIES_COUNT as usize) as u8
}

/// A named per-bar series (e.g. an `sma_20` overlay or an `rsi_14` panel). `values` aligns to the
/// bars (one entry per bar; `None`/non-finite during warm-up).
#[derive(Debug, Clone)]
pub struct Series {
    pub label: String,
    pub values: Vec<Option<f64>>,
}

/// Default chart width in pixels.
pub const DEFAULT_WIDTH: u32 = 1920;
/// Default chart height in pixels.
pub const DEFAULT_HEIGHT: u32 = 1080;

/// Rendering options for [`render`].
#[derive(Debug, Clone)]
pub struct PlotOptions {
    pub width: u32,
    pub height: u32,
    /// Title drawn at the top-left (e.g. `"AAPL 1d"`).
    pub title: String,
    /// Timezone the x-axis calendar boundaries and clock labels are expressed in. This should match
    /// the session tz the bars were aggregated against, so day/month/year ticks land on the same
    /// local boundaries the buckets were anchored to.
    pub tz: TimeZone,
    /// Price-scale overlays (e.g. `sma`/`ema`), drawn as lines on the main price panel.
    pub overlays: Vec<Series>,
    /// Own-scale indicators (e.g. `rsi`/`ret`/`vol`), each drawn in its own stacked lower panel.
    pub panels: Vec<Series>,
    /// Add a volume sub-panel below the price panel.
    pub volume: bool,
    /// Volume-scale overlays (e.g. `vsma`/`vema`), drawn as lines on the volume panel. A non-empty
    /// list implies the volume panel even when [`Self::volume`] is `false`.
    pub volume_overlays: Vec<Series>,
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            title: String::new(),
            tz: TimeZone::UTC,
            overlays: Vec::new(),
            panels: Vec::new(),
            volume: false,
            volume_overlays: Vec::new(),
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

    /// Draws a `t`-thick polyline through `points`, connecting consecutive `Some` points and
    /// breaking the line at any `None` (an indicator's warm-up gap).
    fn polyline(&mut self, points: &[Option<(i32, i32)>], t: i32, color: u8) {
        let mut prev: Option<(i32, i32)> = None;
        for point in points {
            match point {
                Some(p) => {
                    if let Some(q) = prev {
                        self.line(q.0, q.1, p.0, p.1, t, color);
                    }
                    prev = Some(*p);
                }
                None => prev = None,
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
    let grid_t = (ds.round() as i32).max(1);
    let pen_t = grid_t;
    let line_t = ((2.0 * ds).round() as i32).max(1);
    let label_gap = (8.0 * ds) as i32;
    let text_h = 8 * text_scale;

    // Price extent (candles plus any price-scale overlays) with a small headroom pad.
    let mut pmin = f64::INFINITY;
    let mut pmax = f64::NEG_INFINITY;
    for bar in bars {
        pmin = pmin.min(bar.low);
        pmax = pmax.max(bar.high);
    }
    for s in &opts.overlays {
        for v in s.values.iter().flatten() {
            if v.is_finite() {
                pmin = pmin.min(*v);
                pmax = pmax.max(*v);
            }
        }
    }
    if !pmin.is_finite() || !pmax.is_finite() {
        return canvas;
    }
    let ppad = ((pmax - pmin) * 0.04).max(f64::MIN_POSITIVE);
    pmin -= ppad;
    pmax += ppad;

    // Lower sub-panels: an optional volume panel, then one per indicator *kind*. Indicators that
    // share a scale (e.g. rsi:14 and rsi:28, or ret:log and ret:simple) go in the same pane as
    // separate colored lines rather than in stacked panes.
    let mut subs: Vec<SubPanel> = Vec::new();
    // A volume MA (vsma/vema) implies the volume panel even without an explicit --volume.
    if opts.volume || !opts.volume_overlays.is_empty() {
        let vmax = bars.iter().map(|b| b.volume).max().unwrap_or(0).max(0) as f64;
        subs.push(SubPanel {
            title: "volume".to_string(),
            kind: SubKind::Volume,
            vmin: 0.0,
            vmax: if vmax <= 0.0 { 1.0 } else { vmax * 1.05 },
        });
    }
    // Group panel indicators by kind (the label prefix: rsi/ret/vol/...), preserving first-seen
    // order, so each kind gets a single shared pane.
    for (key, idxs) in group_panels(&opts.panels) {
        let (vmin, vmax) = group_range(&opts.panels, &idxs, &key);
        subs.push(SubPanel {
            title: key,
            kind: SubKind::Lines(idxs),
            vmin,
            vmax,
        });
    }

    // Left gutter sized to the widest label across every panel (prices are usually widest, but a
    // volume "1.2B" or a bare "100" must fit too).
    let price_levels: Vec<f64> = (0..=6)
        .map(|k| pmin + (pmax - pmin) * k as f64 / 6.0)
        .collect();
    let mut max_label_w = price_levels
        .iter()
        .map(|v| fmt_price(*v).chars().count() as i32 * char_w)
        .max()
        .unwrap_or(0);
    for sub in &subs {
        for (_, label) in sub.grid_labels() {
            max_label_w = max_label_w.max(label.chars().count() as i32 * char_w);
        }
    }

    let left = max_label_w + label_gap * 2;
    let right = (14.0 * ds) as i32;
    let top = (22.0 * ds) as i32;
    let bottom = (34.0 * ds) as i32;
    let plot_w = (w as i32 - left - right).max(1);

    let content_top = top;
    let content_bottom = h as i32 - bottom;
    let content_h = (content_bottom - content_top).max(1);

    // Vertical split: the price panel on top, sub-panels stacked below sharing up to ~60%.
    let n_sub = subs.len() as i32;
    let panel_gap = (12.0 * ds) as i32;
    let sub_frac = (0.18 * n_sub as f32).min(0.6);
    let subs_total = (content_h as f32 * sub_frac) as i32;
    let each_sub = if n_sub > 0 {
        ((subs_total - panel_gap * n_sub) / n_sub).max((30.0 * ds) as i32)
    } else {
        0
    };
    let subs_used = each_sub.saturating_mul(n_sub) + panel_gap * n_sub;
    let price_h = (content_h - subs_used).max((60.0 * ds) as i32);
    let price = Panel {
        top: content_top,
        height: price_h,
        vmin: pmin,
        vmax: pmax,
    };

    let style = Style {
        char_w,
        text_scale,
        grid_t,
        line_t,
        left,
        plot_w,
        label_gap,
    };
    let x_of =
        |i: usize| -> i32 { left + ((i as f64 + 0.5) * (plot_w as f64 / n as f64)).round() as i32 };

    // Time-axis boundaries (positions only), chosen to suit the visible span, in the configured tz.
    let (gran, time_ticks) = time_axis_ticks(bars, &opts.tz);
    // Resolve label geometry and drop collisions so labels never overprint each other.
    let min_gap = char_w;
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
    // first-visible after a collision drop still carries the year. Recompute geometry from the
    // final label widths.
    let keep = select_nonoverlapping(&boxes, min_gap);
    let placed: Vec<(usize, i32, String)> = label_ticks(gran, &time_ticks, &keep)
        .into_iter()
        .map(|(i, label)| {
            let tw = label.chars().count() as i32 * char_w;
            let lx = x_of(i).min(w as i32 - tw - label_gap).max(0);
            (i, lx, label)
        })
        .collect();

    // Vertical time grid lines span every panel; drawn first so candles/lines paint on top.
    for (i, _, _) in &placed {
        canvas.fill_rect(x_of(*i), content_top, grid_t, content_h, GRID);
    }

    // ---- price panel: horizontal grid, candles, overlays, legend ----
    let price_grid: Vec<(f64, String)> = price_levels.iter().map(|v| (*v, fmt_price(*v))).collect();
    draw_panel_grid(&mut canvas, &style, &price, &price_grid);

    let body_w = ((plot_w as f64 / n as f64) * 0.62).max(1.5 * ds as f64) as i32;
    let min_body = (1.5 * ds).max(1.0) as i32;
    for (i, bar) in bars.iter().enumerate() {
        let x = x_of(i);
        let up = bar.close >= bar.open;
        let color = if up { UP } else { DOWN };
        let yh = price.y(bar.high);
        let yl = price.y(bar.low);
        canvas.fill_rect(x - pen_t / 2, yh, pen_t.max(1), (yl - yh).max(1), color);
        let yo = price.y(bar.open);
        let yc = price.y(bar.close);
        let bt = yo.min(yc);
        let bh = (yo - yc).abs().max(min_body);
        canvas.fill_rect(x - body_w / 2, bt, body_w.max(1), bh, color);
    }
    for (j, s) in opts.overlays.iter().enumerate() {
        draw_series(
            &mut canvas,
            &style,
            &price,
            &x_of,
            &s.values,
            series_color(j),
        );
    }
    // Overlay legend at the price panel's top-left, each entry in its line color.
    let mut ly = content_top + (4.0 * ds) as i32;
    for (j, s) in opts.overlays.iter().enumerate() {
        canvas.text(
            left + (6.0 * ds) as i32,
            ly,
            &s.label,
            series_color(j),
            text_scale,
        );
        ly += text_h + (2.0 * ds) as i32;
    }

    // ---- sub-panels ----
    let mut y = content_top + price_h;
    for sub in &subs {
        y += panel_gap;
        let panel = Panel {
            top: y,
            height: each_sub,
            vmin: sub.vmin,
            vmax: sub.vmax,
        };
        draw_panel_grid(&mut canvas, &style, &panel, &sub.grid_labels());
        // Legend/title stacked at the panel's top-left. Volume is a single labeled series; an
        // indicator pane lists each of its lines in that line's color.
        let legend_x = left + (6.0 * ds) as i32;
        let mut legend_y = panel.top + (2.0 * ds) as i32;
        match &sub.kind {
            SubKind::Volume => {
                let vbottom = panel.top + panel.height;
                for (i, bar) in bars.iter().enumerate() {
                    let x = x_of(i);
                    let yv = panel.y(bar.volume.max(0) as f64);
                    let color = if bar.close >= bar.open { UP } else { DOWN };
                    canvas.fill_rect(
                        x - body_w / 2,
                        yv,
                        body_w.max(1),
                        (vbottom - yv).max(1),
                        color,
                    );
                }
                // Volume MA overlays (vsma/vema) share the volume scale, drawn over the bars. Their
                // colors continue the cycle after the price overlays and indicator panels.
                let vol_base = opts.overlays.len() + opts.panels.len();
                canvas.text(legend_x, legend_y, &sub.title, TEXT, text_scale);
                legend_y += text_h + (2.0 * ds) as i32;
                for (k, s) in opts.volume_overlays.iter().enumerate() {
                    let color = series_color(vol_base + k);
                    draw_series(&mut canvas, &style, &panel, &x_of, &s.values, color);
                    canvas.text(legend_x, legend_y, &s.label, color, text_scale);
                    legend_y += text_h + (2.0 * ds) as i32;
                }
            }
            SubKind::Lines(idxs) => {
                for &idx in idxs {
                    let color = series_color(opts.overlays.len() + idx);
                    draw_series(
                        &mut canvas,
                        &style,
                        &panel,
                        &x_of,
                        &opts.panels[idx].values,
                        color,
                    );
                    canvas.text(
                        legend_x,
                        legend_y,
                        &opts.panels[idx].label,
                        color,
                        text_scale,
                    );
                    legend_y += text_h + (2.0 * ds) as i32;
                }
            }
        }
        y += each_sub;
    }

    // Time-axis labels along the bottom (on top of everything).
    let label_y = content_bottom + (6.0 * ds) as i32;
    for (_, lx, label) in &placed {
        canvas.text(*lx, label_y, label, TEXT, text_scale);
    }

    // Title.
    if !opts.title.is_empty() {
        canvas.text(left, (3.0 * ds) as i32, &opts.title, TEXT, text_scale);
    }

    canvas
}

/// Shared geometry/style passed to the panel drawing helpers.
struct Style {
    char_w: i32,
    text_scale: i32,
    grid_t: i32,
    line_t: i32,
    left: i32,
    plot_w: i32,
    label_gap: i32,
}

/// A drawable panel region with its own value range.
struct Panel {
    top: i32,
    height: i32,
    vmin: f64,
    vmax: f64,
}

impl Panel {
    /// Maps a value to a y pixel within the panel (`vmax` at the top, `vmin` at the bottom).
    fn y(&self, v: f64) -> i32 {
        let range = (self.vmax - self.vmin).max(1e-12);
        self.top + ((self.vmax - v) / range * self.height as f64).round() as i32
    }
}

/// What a lower sub-panel draws.
#[derive(Debug, Clone)]
enum SubKind {
    /// Volume bars colored by candle direction.
    Volume,
    /// One or more lines for `opts.panels[i]` that share this pane's scale.
    Lines(Vec<usize>),
}

/// A lower sub-panel: a title/kind label, its content kind, and value range.
struct SubPanel {
    title: String,
    kind: SubKind,
    vmin: f64,
    vmax: f64,
}

impl SubPanel {
    /// The `(value, label)` pairs for the panel's horizontal grid lines / gutter labels.
    fn grid_labels(&self) -> Vec<(f64, String)> {
        match self.kind {
            SubKind::Volume => {
                let mx = self.vmax;
                vec![
                    (0.0, fmt_compact(0.0)),
                    (mx * 0.5, fmt_compact(mx * 0.5)),
                    (mx, fmt_compact(mx)),
                ]
            }
            SubKind::Lines(_) if self.title == "rsi" => vec![
                (30.0, "30".to_string()),
                (50.0, "50".to_string()),
                (70.0, "70".to_string()),
            ],
            SubKind::Lines(_) => {
                let mid = (self.vmin + self.vmax) / 2.0;
                vec![
                    (self.vmin, fmt_indicator(self.vmin)),
                    (mid, fmt_indicator(mid)),
                    (self.vmax, fmt_indicator(self.vmax)),
                ]
            }
        }
    }
}

/// The grouping key for an indicator panel: the label prefix before the first `_`/`:` (so
/// `rsi_14` and `rsi_28` share the pane keyed `rsi`).
fn panel_key(label: &str) -> &str {
    label.split(['_', ':']).next().unwrap_or(label)
}

/// Groups panel indicators by [`panel_key`], preserving first-seen order. Returns `(key, indices)`
/// where `indices` point into `panels`; each group becomes one shared sub-panel.
fn group_panels(panels: &[Series]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, s) in panels.iter().enumerate() {
        let key = panel_key(&s.label).to_string();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, idxs)) => idxs.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    groups
}

/// The auto-scaled `(min, max)` value range shared by the panel series at `idxs`. `rsi` is pinned
/// to `0..100`; everything else fits the combined finite values with a small pad.
fn group_range(panels: &[Series], idxs: &[usize], key: &str) -> (f64, f64) {
    if key == "rsi" {
        return (0.0, 100.0);
    }
    let mut mn = f64::INFINITY;
    let mut mx = f64::NEG_INFINITY;
    for &i in idxs {
        for v in panels[i].values.iter().flatten() {
            if v.is_finite() {
                mn = mn.min(*v);
                mx = mx.max(*v);
            }
        }
    }
    if !mn.is_finite() || !mx.is_finite() {
        return (0.0, 1.0);
    }
    if (mx - mn).abs() < 1e-12 {
        let e = mx.abs().max(1.0) * 0.1;
        return (mn - e, mx + e);
    }
    let pad = (mx - mn) * 0.08;
    (mn - pad, mx + pad)
}

/// Draws a panel's horizontal grid lines and right-aligned gutter labels. Each label is clamped to
/// stay fully within the panel's vertical band, so the bottom label of one panel and the top label
/// of the panel below (separated only by the inter-panel gap) never overlap.
fn draw_panel_grid(canvas: &mut Canvas, st: &Style, panel: &Panel, labels: &[(f64, String)]) {
    let text_h = 8 * st.text_scale;
    let y_lo = panel.top;
    let y_hi = panel.top + panel.height - text_h;
    for (v, label) in labels {
        let y = panel.y(*v);
        canvas.fill_rect(st.left, y, st.plot_w, st.grid_t, GRID);
        let tw = label.chars().count() as i32 * st.char_w;
        let ty = (y - text_h / 2).clamp(y_lo, y_hi.max(y_lo));
        canvas.text(st.left - st.label_gap - tw, ty, label, TEXT, st.text_scale);
    }
}

/// Draws a value series as a polyline within a panel, breaking at warm-up gaps.
fn draw_series(
    canvas: &mut Canvas,
    st: &Style,
    panel: &Panel,
    x_of: &dyn Fn(usize) -> i32,
    values: &[Option<f64>],
    color: u8,
) {
    let pts: Vec<Option<(i32, i32)>> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (*v).and_then(|x| x.is_finite().then(|| (x_of(i), panel.y(x)))))
        .collect();
    canvas.polyline(&pts, st.line_t, color);
}

/// Formats a price-axis label (two decimals).
fn fmt_price(v: f64) -> String {
    format!("{v:.2}")
}

/// Formats a large count compactly (`1.2B`, `340.0M`, `12k`, `950`).
fn fmt_compact(v: f64) -> String {
    let a = v.abs();
    let (val, suffix) = if a >= 1e12 {
        (v / 1e12, "T")
    } else if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e3 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    if suffix.is_empty() {
        format!("{v:.0}")
    } else {
        format!("{val:.1}{suffix}")
    }
}

/// Formats an indicator-axis label, picking precision from the magnitude.
fn fmt_indicator(v: f64) -> String {
    let a = v.abs();
    if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    }
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
            title: "TEST 1d".into(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
        };
        let canvas = render(&sample(), &opts);
        assert_eq!(canvas.width, 640);
        assert_eq!(canvas.height, 360);
        assert_eq!(canvas.px.len(), 640 * 360);
    }

    #[test]
    fn render_draws_candles_overlays_and_panels() {
        let bars = sample();
        // An sma overlay (price panel), an rsi panel, and the volume panel exercise the series
        // colors and the sub-panel plumbing.
        let sma = crate::analysis::calc::parse_spec("sma:5").unwrap().unwrap();
        let rsi = crate::analysis::calc::parse_spec("rsi:5").unwrap().unwrap();
        let opts = PlotOptions {
            width: 800,
            height: 500,
            title: "TEST 1d".into(),
            overlays: vec![Series {
                label: sma.name(),
                values: sma.compute(&bars),
            }],
            panels: vec![Series {
                label: rsi.name(),
                values: rsi.compute(&bars),
            }],
            volume: true,
            ..Default::default()
        };
        let canvas = render(&bars, &opts);
        // Candles (up/down), grid, text, and the first two series colors must all appear.
        for color in [GRID, TEXT, UP, DOWN, series_color(0), series_color(1)] {
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
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
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
                title: String::new(),
                tz: jiff::tz::TimeZone::UTC,
                ..Default::default()
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
                title: String::new(),
                tz: jiff::tz::TimeZone::UTC,
                ..Default::default()
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
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
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
    fn panels_group_by_indicator_kind() {
        assert_eq!(panel_key("rsi_14"), "rsi");
        assert_eq!(panel_key("ret_log"), "ret");
        let panels = vec![
            Series {
                label: "rsi_14".into(),
                values: vec![],
            },
            Series {
                label: "vol_20".into(),
                values: vec![],
            },
            Series {
                label: "rsi_28".into(),
                values: vec![],
            },
            Series {
                label: "ret_log".into(),
                values: vec![],
            },
        ];
        // rsi_14 and rsi_28 share one pane; order of first appearance is preserved.
        assert_eq!(
            group_panels(&panels),
            vec![
                ("rsi".to_string(), vec![0, 2]),
                ("vol".to_string(), vec![1]),
                ("ret".to_string(), vec![3]),
            ]
        );
    }

    #[test]
    fn shared_rsi_pane_draws_both_lines() {
        // Two RSI periods must render as two differently-colored lines (one shared pane).
        let bars = sample();
        let mut panels = Vec::new();
        for spec in ["rsi:3", "rsi:5"] {
            let ind = crate::analysis::calc::parse_spec(spec).unwrap().unwrap();
            panels.push(Series {
                label: ind.name(),
                values: ind.compute(&bars),
            });
        }
        let opts = PlotOptions {
            width: 800,
            height: 500,
            panels,
            ..Default::default()
        };
        let canvas = render(&bars, &opts);
        // The two panel lines take the first two series colors (no overlays here).
        assert!(
            canvas.px.contains(&series_color(0)),
            "first rsi line missing"
        );
        assert!(
            canvas.px.contains(&series_color(1)),
            "second rsi line missing"
        );
    }

    #[test]
    fn volume_ma_draws_on_the_volume_pane_and_enables_it() {
        // Bars with real volume so both the bars and the vsma line are meaningful.
        let bars: Vec<Bar> = (0..30u32)
            .map(|i| {
                let mut b = bar(1_735_700_000 + i * 86_400, 100.0, 101.0, 99.0, 100.5);
                b.volume = 1000 + (i as i64 % 5) * 200;
                b
            })
            .collect();
        let vsma = crate::analysis::calc::parse_spec("vsma:5")
            .unwrap()
            .unwrap();
        let opts = PlotOptions {
            width: 800,
            height: 500,
            volume_overlays: vec![Series {
                label: vsma.name(),
                values: vsma.compute(&bars),
            }],
            // Note: volume is left false; a volume overlay must auto-enable the pane.
            ..Default::default()
        };
        let canvas = render(&bars, &opts);
        // The volume pane appears (direction-colored bars) and the vsma line uses the first series
        // color (no price overlays or indicator panels precede it).
        assert!(
            canvas.px.contains(&UP) || canvas.px.contains(&DOWN),
            "volume bars missing (pane not enabled)"
        );
        assert!(
            canvas.px.contains(&series_color(0)),
            "vsma overlay line missing"
        );
    }

    #[test]
    fn panel_labels_stay_within_the_panel_band() {
        // A panel's gutter labels must not spill above or below its own band, so the bottom label
        // of one panel and the top label of the next never collide across the inter-panel gap.
        let mut c = Canvas::new(200, 200, BG);
        let st = Style {
            char_w: 8,
            text_scale: 1,
            grid_t: 1,
            line_t: 1,
            left: 60,
            plot_w: 130,
            label_gap: 8,
        };
        let panel = Panel {
            top: 50,
            height: 40,
            vmin: 0.0,
            vmax: 100.0,
        };
        let labels = vec![
            (0.0, "0".to_string()),
            (50.0, "50".to_string()),
            (100.0, "100".to_string()),
        ];
        draw_panel_grid(&mut c, &st, &panel, &labels);
        let w = c.width as usize;
        for y in 0..c.height as i32 {
            let row_has_text = (0..w).any(|x| c.px[y as usize * w + x] == TEXT);
            if row_has_text {
                assert!(
                    y >= panel.top && y < panel.top + panel.height,
                    "label text at row {y} is outside the panel band [{}, {})",
                    panel.top,
                    panel.top + panel.height
                );
            }
        }
    }

    #[test]
    fn empty_series_yields_blank_canvas_of_requested_size() {
        let opts = PlotOptions {
            width: 320,
            height: 200,
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
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
            title: String::new(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
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
            title: "PNG".into(),
            tz: jiff::tz::TimeZone::UTC,
            ..Default::default()
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
