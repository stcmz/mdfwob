//! TOML configuration for the analysis subcommands.
//!
//! These live under the `[analysis]` table so the same file can also carry the
//! download configuration (`[download]`, `[ibkr]`, `[[stocks]]`, ...). Only the
//! `[analysis]` section is consumed here; CLI flags/tokens override these
//! values, which in turn override the built-in defaults.

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::analysis::session::Session;

/// Default RTH window when none is configured.
pub const DEFAULT_RTH_HOURS: &str = "09:30-16:00";
/// Default extended-hours window when none is configured.
pub const DEFAULT_EXTENDED_HOURS: &str = "04:00-20:00";
/// Default timezone when none is configured.
pub const DEFAULT_TZ: &str = "America/New_York";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisConfig {
    /// Symbol universe to resolve when no positional symbol/path is given.
    pub symbols: Vec<String>,
    /// Directory searched when resolving a bare symbol to `<symbol>.fwob`.
    pub output_dir: Option<PathBuf>,
    /// Regular-trading-hours session window.
    pub rth: SessionSpec,
    /// Extended-hours session window.
    pub extended: SessionSpec,
    pub stat: StatDefaults,
    pub bars: BarsDefaults,
    pub calc: CalcDefaults,
}

impl AnalysisConfig {
    /// Builds the session window for the requested mode, applying the
    /// mode-specific default hours when the spec leaves them blank.
    pub fn session(&self, use_rth: bool) -> Result<Session> {
        let spec = if use_rth { &self.rth } else { &self.extended };
        let default_hours = if use_rth {
            DEFAULT_RTH_HOURS
        } else {
            DEFAULT_EXTENDED_HOURS
        };
        let hours = if spec.hours.trim().is_empty() {
            default_hours
        } else {
            spec.hours.as_str()
        };
        Session::new(&spec.tz, hours)
    }
}

/// A configured session window. Blank `hours` defers to the mode default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSpec {
    pub tz: String,
    pub hours: String,
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self {
            tz: DEFAULT_TZ.to_owned(),
            hours: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatDefaults {
    /// Spacing (seconds) above which an intra-day tick gap is counted.
    pub max_gap: u32,
}

impl Default for StatDefaults {
    fn default() -> Self {
        Self { max_gap: 60 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BarsDefaults {
    pub interval: Option<String>,
    pub fill: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReturnMethod {
    #[default]
    Log,
    Simple,
}

impl ReturnMethod {
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "log" => Some(Self::Log),
            "simple" => Some(Self::Simple),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CalcDefaults {
    pub interval: Option<String>,
    pub method: ReturnMethod,
    pub fill: bool,
    pub annualize: bool,
    pub periods_per_year: f64,
}

impl Default for CalcDefaults {
    fn default() -> Self {
        Self {
            interval: None,
            method: ReturnMethod::Log,
            fill: false,
            annualize: false,
            periods_per_year: 252.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_new_york() {
        let config = AnalysisConfig::default();
        assert!(config.session(true).is_ok());
        assert!(config.session(false).is_ok());
        assert_eq!(config.stat.max_gap, 60);
        assert_eq!(config.calc.method, ReturnMethod::Log);
        assert_eq!(config.calc.periods_per_year, 252.0);
    }

    #[test]
    fn partial_session_keeps_mode_default_hours() {
        let config: AnalysisConfig = toml::from_str(
            r#"
                [extended]
                tz = "America/New_York"
            "#,
        )
        .unwrap();
        // hours left blank => extended default applies, session builds fine.
        assert!(config.session(false).is_ok());
    }
}
