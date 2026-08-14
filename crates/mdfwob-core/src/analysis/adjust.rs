//! Corporate actions and price adjustment.
//!
//! Ticks in an archive are **raw trade prints** — the prices that actually changed hands. A split
//! therefore shows up as a discontinuity: AAPL's 4-for-1 in August 2020 drops the close from
//! $498.90 to $127.62 overnight, which any return calculation reads as a −75% day.
//!
//! Rather than rewriting stored prices, actions are kept in a small hand-auditable table and the
//! adjustment is applied **in memory at read time**. That keeps the raw prints recoverable, avoids
//! rewriting gigabytes when a new split lands, avoids compounding fixed-point rounding, and lets
//! one archive serve raw, split-adjusted, and total-return views of the same data.
//!
//! # Table format
//!
//! Lives under `[actions]`, either in the shared config file or in a file of its own (other
//! sections are ignored, so one file can drive downloads, analysis, and adjustment):
//!
//! ```toml
//! [actions]
//! AAPL  = [{ date = "2020-08-31", split = 4 }]
//! GOOGL = [{ date = "2022-07-18", split = 20 }]
//! AMZN  = [{ date = "2022-06-06", split = 20 }]
//! MSFT  = [{ date = "2024-02-15", dividend = 0.75 }]
//! ```
//!
//! `date` is the **ex-date** in exchange-local time: the first session that trades at the new
//! price. `split` is the ratio of new shares to old (4 means 4-for-1; 0.1 means a 1-for-10
//! reverse). `dividend` is cash per share.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use jiff::civil;
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::analysis::model::Bar;
use crate::normalize_symbol;

/// What happened to the share class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionKind {
    /// New shares per old share: 4 is a 4-for-1 split, 0.1 a 1-for-10 reverse split.
    Split { ratio: f64 },
    /// Cash paid per share.
    CashDividend { amount: f64 },
}

/// A corporate action, stamped at the ex-date's exchange-local midnight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorporateAction {
    pub time: u32,
    pub kind: ActionKind,
}

/// Which adjustments to apply when reading prices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdjustmentMode {
    /// Exactly what traded. Splits appear as discontinuities.
    Raw,
    /// Back-adjust splits only — the right default for price-based research.
    #[default]
    SplitOnly,
    /// Splits plus dividends reinvested, giving a total-return series.
    TotalReturn,
}

impl AdjustmentMode {
    /// Parses a CLI token. Returns `None` for anything unrecognized so a token classifier can
    /// fall through to paths and symbols.
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "raw" => Some(Self::Raw),
            "split-only" | "splits" => Some(Self::SplitOnly),
            "total-return" | "total" => Some(Self::TotalReturn),
            _ => None,
        }
    }
}

/// One row of the `[actions]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// Ex-date as `YYYY-MM-DD`, in the exchange timezone.
    pub date: String,
    /// New shares per old share.
    #[serde(default)]
    pub split: Option<f64>,
    /// Cash per share.
    #[serde(default)]
    pub dividend: Option<f64>,
}

/// The `[actions]` table: symbol -> actions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct ActionTable(BTreeMap<String, Vec<ActionSpec>>);

/// Tolerant view of a config file: everything except `[actions]` is ignored, so the same file can
/// also carry `[analysis]`, `[download]`, and the rest.
#[derive(Debug, Default, Deserialize)]
struct ActionsFile {
    #[serde(default)]
    actions: ActionTable,
}

impl ActionTable {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Resolves one symbol's actions into epoch-stamped form, sorted ascending.
    ///
    /// Symbol lookup is case-insensitive. An unknown symbol yields an empty list — a symbol with
    /// no corporate actions is the common case, not an error.
    pub fn resolve(&self, symbol: &str, tz: &TimeZone) -> Result<Vec<CorporateAction>> {
        let wanted = normalize_symbol(symbol);
        let Some((_, specs)) = self
            .0
            .iter()
            .find(|(key, _)| normalize_symbol(key) == wanted)
        else {
            return Ok(Vec::new());
        };

        let mut out = Vec::with_capacity(specs.len());
        for spec in specs {
            let kind = match (spec.split, spec.dividend) {
                (Some(_), Some(_)) => bail!(
                    "{symbol} action on {}: set exactly one of `split` or `dividend`",
                    spec.date
                ),
                (None, None) => bail!(
                    "{symbol} action on {}: needs a `split` or a `dividend`",
                    spec.date
                ),
                (Some(ratio), None) => {
                    if !(ratio.is_finite() && ratio > 0.0) || (ratio - 1.0).abs() < f64::EPSILON {
                        bail!(
                            "{symbol} split on {}: ratio must be positive and != 1, got {ratio}",
                            spec.date
                        );
                    }
                    ActionKind::Split { ratio }
                }
                (None, Some(amount)) => {
                    if !(amount.is_finite() && amount > 0.0) {
                        bail!(
                            "{symbol} dividend on {}: amount must be positive, got {amount}",
                            spec.date
                        );
                    }
                    ActionKind::CashDividend { amount }
                }
            };

            let date: civil::Date = spec.date.parse().with_context(|| {
                format!(
                    "{symbol}: invalid ex-date {:?}, expected YYYY-MM-DD",
                    spec.date
                )
            })?;
            let secs = date
                .to_zoned(tz.clone())
                .with_context(|| {
                    format!("{symbol}: ex-date {} is not a valid local time", spec.date)
                })?
                .timestamp()
                .as_second();
            let time = u32::try_from(secs)
                .with_context(|| format!("{symbol}: ex-date {} out of range", spec.date))?;

            out.push(CorporateAction { time, kind });
        }
        out.sort_by_key(|action| action.time);
        Ok(out)
    }
}

/// Reads an `[actions]` table from a TOML file, ignoring every other section.
pub fn load_actions(path: &Path) -> Result<ActionTable> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read actions from {}", path.display()))?;
    let file: ActionsFile =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(file.actions)
}

/// Back-adjusts `bars` in place for `actions`, which must be sorted ascending by time.
///
/// Walks backwards accumulating one cumulative factor per bar, so each price is scaled exactly
/// once no matter how many actions precede it — repeated multiplication would compound rounding.
/// Bars on or after an ex-date are left alone: they already trade at the new price.
pub fn adjust_bars(bars: &mut [Bar], actions: &[CorporateAction], mode: AdjustmentMode) {
    if mode == AdjustmentMode::Raw || bars.is_empty() || actions.is_empty() {
        return;
    }

    let mut price = 1.0f64;
    let mut volume = 1.0f64;
    let mut next = actions.len();

    for i in (0..bars.len()).rev() {
        // Fold in every action whose ex-date is strictly after this bar.
        while next > 0 && actions[next - 1].time > bars[i].time {
            next -= 1;
            match actions[next].kind {
                ActionKind::Split { ratio } => {
                    price /= ratio;
                    volume *= ratio;
                }
                ActionKind::CashDividend { amount } => {
                    if mode == AdjustmentMode::TotalReturn {
                        // `bars[i]` is the last bar before the ex-date and is still unadjusted at
                        // this point, so this is the raw close the dividend detached from. The
                        // factor is a ratio, hence scale-free and safe to compose with splits.
                        let close = bars[i].close;
                        if close > 0.0 && amount < close {
                            price *= (close - amount) / close;
                        }
                    }
                }
            }
        }

        if price != 1.0 {
            let bar = &mut bars[i];
            bar.open *= price;
            bar.high *= price;
            bar.low *= price;
            bar.close *= price;
            if bar.vwap.is_finite() {
                bar.vwap *= price;
            }
        }
        if volume != 1.0 {
            bars[i].volume = (bars[i].volume as f64 * volume).round() as i64;
        }
    }
}

/// Adjusts bars as they stream, in one forward pass.
///
/// A split needs no market data at all: the factor for a bar is the product of `1/ratio` over every
/// split with a *later* ex-date, which the action table alone determines. Since an action table
/// holds a handful of rows and bars arrive in ascending time, a cursor over those rows gives each
/// bar its factor with no buffering and no extra I/O — so even a whole-history conversion at `1s`
/// can be adjusted at constant memory.
///
/// Dividends are the exception, and the reason [`Adjuster::streaming`] refuses
/// [`AdjustmentMode::TotalReturn`]: their factor is `(C - D) / C`, where `C` is the close
/// immediately *before* the ex-date. That price is not in the table, and the bars needing it are
/// emitted before a forward pass ever reaches it. Use [`adjust_bars`] over a materialized series
/// for total-return.
#[derive(Debug, Clone, Default)]
pub struct Adjuster {
    /// Ascending by ex-date: `(ex_time, price_factor, volume_factor)`. A bar strictly before
    /// `ex_time` — and at or after the previous entry's — takes these factors.
    bounds: Vec<(u32, f64, f64)>,
    cursor: usize,
}

impl Adjuster {
    /// Builds an adjuster usable in a streaming pass.
    ///
    /// Errors for [`AdjustmentMode::TotalReturn`], which cannot be done forward-only.
    pub fn streaming(actions: &[CorporateAction], mode: AdjustmentMode) -> Result<Self> {
        match mode {
            AdjustmentMode::Raw => Ok(Self::default()),
            AdjustmentMode::SplitOnly => Ok(Self::splits(actions)),
            AdjustmentMode::TotalReturn => bail!(
                "total-return adjustment needs the close before each ex-date, which a forward \
                 streaming pass cannot see; use split-only here, or adjust a materialized series"
            ),
        }
    }

    /// Split-only adjustment. `actions` must be sorted ascending by time; non-splits are ignored.
    pub fn splits(actions: &[CorporateAction]) -> Self {
        let mut bounds = Vec::new();
        let (mut price, mut volume) = (1.0f64, 1.0f64);
        // Walk backwards: a bar before this ex-date carries this action's factor and every later
        // one's, so the cumulative product falls out in a single reverse pass.
        for action in actions.iter().rev() {
            if let ActionKind::Split { ratio } = action.kind {
                price /= ratio;
                volume *= ratio;
            }
            bounds.push((action.time, price, volume));
        }
        bounds.reverse();
        Self { bounds, cursor: 0 }
    }

    /// True when this adjuster would leave every bar untouched.
    pub fn is_identity(&self) -> bool {
        self.bounds.is_empty()
    }

    /// Scales one bar. Bars must be supplied in ascending time order.
    pub fn apply(&mut self, bar: &mut Bar) {
        // Advance past every ex-date this bar is at or after; those no longer apply to it.
        while self.cursor < self.bounds.len() && self.bounds[self.cursor].0 <= bar.time {
            self.cursor += 1;
        }
        let Some(&(_, price, volume)) = self.bounds.get(self.cursor) else {
            return; // at or after the last ex-date: already in current terms
        };
        if price != 1.0 {
            bar.open *= price;
            bar.high *= price;
            bar.low *= price;
            bar.close *= price;
            if bar.vwap.is_finite() {
                bar.vwap *= price;
            }
        }
        if volume != 1.0 {
            bar.volume = (bar.volume as f64 * volume).round() as i64;
        }
    }
}

/// Infers splits from overnight price gaps near a clean integer ratio.
///
/// A **bootstrap and audit aid, not the live path**: it cannot see dividends, and a genuine
/// overnight crash of the right size is indistinguishable from a split. Use it to draft an
/// `[actions]` table, then check the result against the issuer's record.
pub fn detect_splits(bars: &[Bar]) -> Vec<CorporateAction> {
    const MIN_RATIO: f64 = 1.5;
    const TOLERANCE: f64 = 0.05;

    let mut out = Vec::new();
    for pair in bars.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if cur.open <= 0.0 || prev.close <= 0.0 {
            continue;
        }
        let gap = prev.close / cur.open;
        let ratio = if gap > MIN_RATIO {
            // Forward split: price fell by a whole-number factor.
            let f = gap.round();
            (f >= 2.0 && ((gap - f) / f).abs() < TOLERANCE).then_some(f)
        } else if gap < 1.0 / MIN_RATIO {
            // Reverse split: price rose by a whole-number factor.
            let inv = 1.0 / gap;
            let f = inv.round();
            (f >= 2.0 && ((inv - f) / f).abs() < TOLERANCE).then_some(1.0 / f)
        } else {
            None
        };
        if let Some(ratio) = ratio {
            out.push(CorporateAction {
                time: cur.time,
                kind: ActionKind::Split { ratio },
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(time: u32, close: f64, volume: i64) -> Bar {
        Bar {
            time,
            open: close,
            high: close,
            low: close,
            close,
            volume,
            vwap: close,
            trades: 1,
        }
    }

    const DAY: u32 = 86_400;

    #[test]
    fn a_split_makes_the_series_continuous() {
        // 100, 100, then a 4-for-1: 25, 25. Back-adjusted, every bar should read 25.
        let mut bars = vec![
            bar(DAY, 100.0, 400),
            bar(2 * DAY, 100.0, 400),
            bar(3 * DAY, 25.0, 1600),
            bar(4 * DAY, 25.0, 1600),
        ];
        let actions = [CorporateAction {
            time: 3 * DAY,
            kind: ActionKind::Split { ratio: 4.0 },
        }];
        adjust_bars(&mut bars, &actions, AdjustmentMode::SplitOnly);

        for b in &bars {
            assert!((b.close - 25.0).abs() < 1e-9, "close {}", b.close);
            assert_eq!(b.volume, 1600, "volume is scaled the other way");
        }
    }

    #[test]
    fn the_ex_date_bar_is_left_alone() {
        let mut bars = vec![bar(DAY, 100.0, 100), bar(2 * DAY, 25.0, 400)];
        let actions = [CorporateAction {
            time: 2 * DAY,
            kind: ActionKind::Split { ratio: 4.0 },
        }];
        adjust_bars(&mut bars, &actions, AdjustmentMode::SplitOnly);
        assert!((bars[1].close - 25.0).abs() < 1e-9, "already post-split");
        assert!((bars[0].close - 25.0).abs() < 1e-9, "pre-split scaled down");
    }

    #[test]
    fn consecutive_splits_compose() {
        // 4-for-1 then 5-for-1 => everything before both is divided by 20.
        let mut bars = vec![
            bar(DAY, 200.0, 10),
            bar(2 * DAY, 50.0, 40),
            bar(3 * DAY, 10.0, 200),
        ];
        let actions = [
            CorporateAction {
                time: 2 * DAY,
                kind: ActionKind::Split { ratio: 4.0 },
            },
            CorporateAction {
                time: 3 * DAY,
                kind: ActionKind::Split { ratio: 5.0 },
            },
        ];
        adjust_bars(&mut bars, &actions, AdjustmentMode::SplitOnly);
        assert!((bars[0].close - 10.0).abs() < 1e-9, "200/20");
        assert!((bars[1].close - 10.0).abs() < 1e-9, "50/5");
        assert!((bars[2].close - 10.0).abs() < 1e-9);
        assert_eq!(bars[0].volume, 200);
    }

    #[test]
    fn a_reverse_split_scales_prices_up() {
        let mut bars = vec![bar(DAY, 1.0, 1000), bar(2 * DAY, 10.0, 100)];
        let actions = [CorporateAction {
            time: 2 * DAY,
            kind: ActionKind::Split { ratio: 0.1 },
        }];
        adjust_bars(&mut bars, &actions, AdjustmentMode::SplitOnly);
        assert!((bars[0].close - 10.0).abs() < 1e-9);
        assert_eq!(bars[0].volume, 100);
    }

    #[test]
    fn dividends_apply_only_in_total_return_mode() {
        let actions = [CorporateAction {
            time: 2 * DAY,
            kind: ActionKind::CashDividend { amount: 1.0 },
        }];

        let mut split_only = vec![bar(DAY, 100.0, 10), bar(2 * DAY, 99.0, 10)];
        adjust_bars(&mut split_only, &actions, AdjustmentMode::SplitOnly);
        assert!((split_only[0].close - 100.0).abs() < 1e-9, "untouched");

        let mut total = vec![bar(DAY, 100.0, 10), bar(2 * DAY, 99.0, 10)];
        adjust_bars(&mut total, &actions, AdjustmentMode::TotalReturn);
        // factor = (100 - 1)/100 = 0.99
        assert!((total[0].close - 99.0).abs() < 1e-9, "{}", total[0].close);
        assert_eq!(total[0].volume, 10, "dividends do not touch volume");
    }

    #[test]
    fn raw_mode_changes_nothing() {
        let original = vec![bar(DAY, 100.0, 10), bar(2 * DAY, 25.0, 40)];
        let mut bars = original.clone();
        let actions = [CorporateAction {
            time: 2 * DAY,
            kind: ActionKind::Split { ratio: 4.0 },
        }];
        adjust_bars(&mut bars, &actions, AdjustmentMode::Raw);
        assert_eq!(bars[0].close, original[0].close);
    }

    #[test]
    fn detect_splits_finds_forward_and_reverse_and_ignores_ordinary_moves() {
        let bars = vec![
            bar(DAY, 100.0, 10),
            bar(2 * DAY, 25.0, 40), // 4-for-1
            bar(3 * DAY, 20.0, 40), // -20%, an ordinary drop
            bar(4 * DAY, 200.0, 4), // 1-for-10 reverse
            bar(5 * DAY, 150.0, 4), // -25%, ordinary
        ];
        let found = detect_splits(&bars);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].kind, ActionKind::Split { ratio: 4.0 });
        assert_eq!(found[1].time, 4 * DAY);
        match found[1].kind {
            ActionKind::Split { ratio } => assert!((ratio - 0.1).abs() < 1e-9, "{ratio}"),
            other => panic!("expected a split, got {other:?}"),
        }
    }

    #[test]
    fn a_crash_short_of_the_threshold_is_not_called_a_split() {
        // META fell 26% in a single session in Feb 2022; that must not read as a split.
        let bars = vec![bar(DAY, 323.0, 10), bar(2 * DAY, 237.76, 10)];
        assert!(detect_splits(&bars).is_empty());
    }

    fn table(toml_text: &str) -> ActionTable {
        let file: ActionsFile = toml::from_str(toml_text).unwrap();
        file.actions
    }

    #[test]
    fn unrelated_sections_are_ignored_so_one_file_serves_every_purpose() {
        let t = table(
            r#"
                [analysis]
                symbols = ["AAPL"]
                [download]
                output_dir = "/tmp"
                [actions]
                AAPL = [{ date = "2020-08-31", split = 4 }]
            "#,
        );
        let actions = t.resolve("aapl", &TimeZone::UTC).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].kind, ActionKind::Split { ratio: 4.0 });
    }

    #[test]
    fn a_symbol_with_no_actions_resolves_to_nothing() {
        let t = table("[actions]\nAAPL = [{ date = \"2020-08-31\", split = 4 }]");
        assert!(t.resolve("MSFT", &TimeZone::UTC).unwrap().is_empty());
    }

    #[test]
    fn actions_are_returned_in_chronological_order() {
        let t = table(
            r#"
                [actions]
                X = [
                  { date = "2022-07-18", split = 20 },
                  { date = "2020-08-31", split = 4 },
                ]
            "#,
        );
        let actions = t.resolve("X", &TimeZone::UTC).unwrap();
        assert!(actions[0].time < actions[1].time);
    }

    #[test]
    fn a_malformed_row_is_rejected_rather_than_guessed_at() {
        let both = table("[actions]\nX = [{ date = \"2020-01-02\", split = 4, dividend = 1 }]");
        assert!(both.resolve("X", &TimeZone::UTC).is_err(), "ambiguous row");

        let neither = table("[actions]\nX = [{ date = \"2020-01-02\" }]");
        assert!(neither.resolve("X", &TimeZone::UTC).is_err(), "no action");

        let bad_date = table("[actions]\nX = [{ date = \"not-a-date\", split = 2 }]");
        assert!(bad_date.resolve("X", &TimeZone::UTC).is_err());

        let bad_ratio = table("[actions]\nX = [{ date = \"2020-01-02\", split = 0 }]");
        assert!(bad_ratio.resolve("X", &TimeZone::UTC).is_err());
    }

    /// The streaming adjuster must agree with the buffered one bar for bar; otherwise the CLI and
    /// a materialized-series consumer would quietly disagree about the same data.
    #[test]
    fn streaming_matches_the_buffered_adjustment() {
        let actions = [
            CorporateAction {
                time: 3 * DAY,
                kind: ActionKind::Split { ratio: 4.0 },
            },
            CorporateAction {
                time: 6 * DAY,
                kind: ActionKind::Split { ratio: 5.0 },
            },
        ];
        let series: Vec<Bar> = (1..=8)
            .map(|i| bar(i * DAY, 100.0 + i as f64, 100 * i as i64))
            .collect();

        let mut buffered = series.clone();
        adjust_bars(&mut buffered, &actions, AdjustmentMode::SplitOnly);

        let mut streamed = series;
        let mut adj = Adjuster::streaming(&actions, AdjustmentMode::SplitOnly).unwrap();
        for b in &mut streamed {
            adj.apply(b);
        }

        for (a, b) in buffered.iter().zip(&streamed) {
            assert_eq!(a.time, b.time);
            assert_eq!(a.open.to_bits(), b.open.to_bits(), "open at {}", a.time);
            assert_eq!(a.close.to_bits(), b.close.to_bits(), "close at {}", a.time);
            assert_eq!(a.volume, b.volume, "volume at {}", a.time);
        }
        // Sanity: the first bar really is divided by 20.
        assert!((streamed[0].close - 101.0 / 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_streaming_adjuster_needs_no_market_data_for_splits() {
        let actions = [CorporateAction {
            time: 2 * DAY,
            kind: ActionKind::Split { ratio: 4.0 },
        }];
        let mut adj = Adjuster::streaming(&actions, AdjustmentMode::SplitOnly).unwrap();
        assert!(!adj.is_identity());

        let mut before = bar(DAY, 100.0, 100);
        let mut on_ex = bar(2 * DAY, 25.0, 400);
        adj.apply(&mut before);
        adj.apply(&mut on_ex);
        assert!((before.close - 25.0).abs() < 1e-9, "pre-split scaled down");
        assert_eq!(before.volume, 400);
        assert!((on_ex.close - 25.0).abs() < 1e-9, "ex-date bar untouched");
        assert_eq!(on_ex.volume, 400);
    }

    #[test]
    fn raw_streams_as_the_identity_and_total_return_is_refused() {
        let actions = [CorporateAction {
            time: DAY,
            kind: ActionKind::CashDividend { amount: 1.0 },
        }];
        assert!(
            Adjuster::streaming(&actions, AdjustmentMode::Raw)
                .unwrap()
                .is_identity()
        );
        // Total-return needs the pre-ex close, which a forward pass cannot have.
        assert!(Adjuster::streaming(&actions, AdjustmentMode::TotalReturn).is_err());
    }

    #[test]
    fn mode_tokens_parse() {
        assert_eq!(AdjustmentMode::from_token("raw"), Some(AdjustmentMode::Raw));
        assert_eq!(
            AdjustmentMode::from_token("split-only"),
            Some(AdjustmentMode::SplitOnly)
        );
        assert_eq!(
            AdjustmentMode::from_token("total-return"),
            Some(AdjustmentMode::TotalReturn)
        );
        assert_eq!(AdjustmentMode::from_token("AAPL"), None, "not a mode token");
        assert_eq!(AdjustmentMode::default(), AdjustmentMode::SplitOnly);
    }
}
