# mdfwob

[![CI](https://github.com/stcmz/mdfwob/actions/workflows/ci.yml/badge.svg)](https://github.com/stcmz/mdfwob/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/mdfwob.svg)](https://crates.io/crates/mdfwob)

Standalone CLI for downloading historical market data into FWOB files.

The output tick schema is:

```text
time   u32  Unix seconds since the UTC epoch
price  u32  price * 10,000  (4 fixed decimals)
size   i32  shares
```

v2 files tag display semantics so `fwob cat` is human-readable: `time` as
`unix-seconds` (shown as an RFC3339 datetime) and `price` as `fixed-4` (shown
divided by 10,000, e.g. `150.0000`). The stored integers are unchanged; the
semantics only affect display.

`time` is always an absolute UTC epoch second. TWS login timezone, computer
timezone, exchange timezone, and daylight-saving transitions do not change the
stored value. Date-time inputs must include an RFC3339 offset or `Z`; date-only
inputs mean midnight UTC.

FWOB write defaults intentionally follow the `fwob convert` defaults:

```text
v2 512KiB columnar-basic zstd --zstd-level 6
```

Optional FWOB tokens can override those defaults:

```text
mdfwob download AAPL MSFT v2 1MiB columnar-delta zstd
mdfwob download AAPL MSFT v1
```

All positional tokens are case-sensitive. Lowercase `v2` and `zstd` are
tokens; uppercase `V2` and `ZSTD` are symbols. Provider tokens are also exact
lowercase tokens, so the stock symbol `IBKR` does not conflict with the `ibkr`
provider token.

Existing files are auto-detected. A v1 file remains v1 when resumed even when
the `v1` token is omitted. The format token controls creation of new files.
When an existing v2 file is resumed without writer-tuning tokens, FWOB
inherits its codec and encoding. Supplying codec, encoding, packing, page-size,
partial-page, or zstd-level options explicitly overrides the inherited write
settings for newly appended pages.
Writes use the version-neutral `fwob::Writer` API. Each downloaded batch is
validated transactionally before any frame is appended, including frame size
and timestamp ordering.

Each output is protected by an OS-level `*.fwob.lock` sidecar for the duration
of verification, repair, and download. A second `mdfwob` process targeting the
same file exits with an error instead of writing concurrently. Lock files may
remain on disk after exit; ownership is determined by the OS lock, not by the
file's existence.

At startup, existing files receive the lightweight structural check provided
by `fwob::Maintenance`. Full verification and committed-tail repair run only
when that check detects corruption, such as an interrupted append. To request
a full scan explicitly:

```text
mdfwob verify AAPL.fwob
```

The verify command supports both FWOB v1 and v2.

Config-driven batch run:

```text
mdfwob download contracts.example.toml
```

The config supports provider-specific `[ibkr]` and `[databento]` settings,
provider-neutral `[download]` settings, grouped
`[[stocks]]` entries, and exact `[[options]]` contracts. Options require
`symbol`, `expiration` (`YYYYMMDD`), `strike`, and `right` (`call`, `put`, `C`,
or `P`). Currency, exchange, and multiplier default to `USD`, `SMART`, and
`100`; `trading_class` and `local_symbol` can disambiguate weekly or nonstandard
contracts. Option output names include the expiry, right, and strike, for
example `MSFT_20260717_C_450.fwob`. See `contracts.example.toml` for the
complete schema. Historical request starts are globally spaced by 1 second by
default; set `download.request_interval_ms` to override this. Unknown config
keys, unsafe output names, and contracts that map to the same output filename
are rejected before a provider connection is opened.

`download.provider` defaults to `ibkr`. Override it with a provider token:

```text
mdfwob download contracts.example.toml databento
mdfwob download databento AAPL --start 2024-01-01
mdfwob download IBKR
```

`ibkr` and `databento` are implemented. `polygon` and `thetadata` are reserved
provider tokens but are not implemented.

IBKR connection resets are retried from the unchanged download cursor. Set
`ibkr.reconnect_timeout_seconds` or `--reconnect-timeout-seconds` to `-1` for
unlimited retries (the default), `0` to fail the symbol immediately, or a
positive number for a wall-clock retry budget in seconds. Retry attempts are
paced by their own global interval, `download.retry_interval_ms` /
`--retry-interval-ms` (default 10000), independent of the normal data-fetch
spacing in `download.request_interval_ms` (default 1000).

The same budget governs the initial connection: if TWS/IB Gateway is still
starting up or has yet to accept the paper-trading disclaimer, mdfwob waits and
retries on the `retry_interval_ms` cadence rather than exiting on the first
failure, so it can be launched alongside the gateway. With the default `-1` it
waits indefinitely for the gateway to become ready; `0` fails fast.

A download keeps one writer open per symbol and durably flushes it to disk
periodically so the output advances live and a crash loses at most the ticks since
the last commit, rather than buffering a whole multi-month backlog until the symbol
completes. A commit is a checkpoint on the still-open writer (never a reopen), so the
resulting file is byte-for-byte identical regardless of how often it commits.
`download.commit_interval_seconds` / `--commit-interval-seconds` controls the cadence
(default 60): `-1` commits only at the end (and on Ctrl+C), `0` commits after every
batch, and a positive value commits at most that often.

A TWS/IB Gateway upstream-connectivity blip (IBKR system codes 1100 then
1101/1102) can silently orphan an in-flight request without dropping the API
socket. A background watcher on the IBKR notice stream detects the restore and
re-issues the affected request from the unchanged cursor within about a second,
so the download resumes on its own without a gateway restart. Signing into the same IBKR account from another device severs TWS's link to the
IBKR servers and is handled two ways. A request issued while that competing
session is active is rejected outright with IBKR code 10187 ("Trading TWS session
is connected from a different IP address"); rather than abandoning the symbol,
mdfwob retries it from the unchanged cursor (paced, within the reconnect budget)
until the other session ends. A request already in flight can instead be wedged
silently — no socket error and no 1101/1102 restore notice — so a request that
makes no progress at all for `ibkr.stall_timeout_seconds` (default 60; set
`--stall-timeout-seconds`, or `0` to disable) is treated as stalled and re-issued
on the existing connection from the unchanged cursor, so the download recovers on
its own instead of hanging until `mdfwob` is restarted. (A stalled request is not
reconnected: the socket is usually still alive — it is serving other symbols — and
the orphan is upstream, so a reconnect with the same client id would only collide
with itself; a genuinely dead socket instead surfaces a connection-loss error that
does trigger a reconnect.) A request that is merely slow but still receiving data
or notices is never re-issued. Ctrl+C is honored promptly even while a request is
blocked or stalled; a second Ctrl+C forces an immediate exit.

Databento uses its official Rust SDK and reads the API key from
`DATABENTO_API_KEY` by default. The variable name and stock/option datasets are
configured in `[databento]`. Databento requires an explicit `download.start`
or `--start`, downloads at most one day per request, and currently supports
all-hours data only. Databento option entries must specify `local_symbol`.
Databento SDK access is currently serialized and each request buffers up to one
day of trades in memory, so increasing `download.parallelism` does not create
concurrent Databento requests.

`download.end` is optional. When specified, it is a fixed RFC3339 cutoff. When
omitted, the downloader evaluates the current UTC time dynamically as each
symbol/request is processed, so long-running batch downloads do not use a stale
startup timestamp.

Ad hoc run:

```text
mdfwob download AAPL MSFT --output D:/MarketData
mdfwob download SPCX --request-interval-ms 1000
```

## Analysis

Four subcommands analyze the FWOB files mdfwob produces. They read both the tick
files it downloads and the bar files `bars --format fwob` writes. They share the
fwob-family positional-token style: paths/symbols, an interval token
(`Ns`/`Nm`/`Nh`/`Nd`/`Nw`/`Nmo`/`Ny` for seconds/minutes/hours/days/weeks/
months/years), an output-format token (`table` default, plus `csv`, `md`,
`jsonl`, `raw`, `hex`, and — for `bars`/`calc` — `fwob`), and the `rth` token
(equivalent to `--use-rth`). A bare symbol is resolved to
`<output_dir>/<symbol>.fwob`; a directory contributes its immediate `*.fwob`
files; no path uses the current directory. `--start`/`--end` accept a date
(`YYYY-MM-DD`, UTC) or RFC3339; a bare end date is inclusive of that day.

```text
mdfwob stat data/ md rth
mdfwob bars AAPL.fwob                  # 1d bars (the default period)
mdfwob bars AAPL.fwob 5m
mdfwob bars AAPL.fwob 1w               # weekly bars (calendar week, exchange tz)
mdfwob bars data/ 1mo fwob --output bars/
mdfwob bars AAPL.fwob 5m rth fill            # RTH bars, forward-fill gaps in-session
mdfwob bars AAPL.fwob 1h 2026-01-01..2026-02-01  # range token (exchange tz, end exclusive)
mdfwob calc AAPL.fwob 1d rth ret:log vol:20 sma:20 rsi:14 --summary
mdfwob calc AAPL.fwob sma:20                 # no interval token -> 1d (the default)
mdfwob plot AAPL.fwob 5m rsi:14 volume       # candlesticks to the console (Sixel)
mdfwob plot AAPL.fwob 1d sma:50 -o chart.png # ...or write a PNG
```

- **`stat`** prints one summary row per file (tick or bar): symbol, `kind`
  (`tick`/`bar`), format, trade count, time range, price min/max, VWAP, and
  signed volume. Every field is derivable from either format, so a tick file and
  the bars it resamples into report the same stats.
- **`bars`** resamples ticks — or re-resamples coarser bars (e.g. `1s`→`1m`) —
  into OHLCV(+VWAP, trades) bars, streaming each row to stdout as its bucket
  closes; with no interval token it defaults to `1d`.
  **Sub-day intervals (s/m/h) are anchored to the
  session open** in the exchange timezone, so e.g. RTH `1h` bars start 09:30,
  10:30, …; **day, week, month, and year intervals are calendar-aligned in the
  exchange timezone** (DST-correct) — week to Monday, month to the 1st, year to
  Jan 1 — so extended-hours ticks that cross UTC midnight stay in the correct
  period's bar. An interval that does not evenly divide the active session
  (RTH 6h30m, extended 16h) is warned about, since the session's last bar is then
  shorter than the rest. The `fill` token forward-fills empty intervals within a
  session (never across the overnight gap); `fwob` output writes one
  `<symbol>.fwob` per symbol into `--output`.
- **`calc`** computes per-bar indicator columns from composable specs —
  `sma:N`, `ema:N`, `dema:N` (moving averages of close); `vsma:N`, `vema:N`,
  `vdema:N` (the same, of **volume** — the `v` prefix means volume); `rsi:N`;
  `ret:log`, `ret:simple`; and `vol:N` (rolling realized **volatility**, the stdev
  of log returns) — over tick or bar files, both resampled/re-resampled to the
  interval token (default `1d` when none is given, like `bars`/`plot`). Note the
  naming quirk: standalone `vol` is *volatility*, while the `v` *prefix* on
  `vsma`/`vema`/`vdema` is *volume*. Rows stream to stdout as each bar closes (like
  `bars`), so the built-in indicators are computed incrementally rather than
  buffering the whole series. Columns are stored as 4-byte fixed-point integers at
  a per-indicator precision (price-level indicators use 4 decimals,
  returns/volatility use 8); warm-up cells with no value are shown as `-`.
  `--summary` appends a footer as **colored TOML** (the same style as `fwob
  inspect`): a `[summary.price]` block (first/last/high/low, `drawdown_from_peak`,
  `cagr`), a `[summary.<col>]` block per indicator with `n/mean/min/max/last`, and
  — only when a `ret:log`/`ret:simple` column is present — a `[summary.ret_*]`
  block summarizing that return series as a fitted normal (`mean`, `stdev`, `skew`,
  `excess_kurtosis`, `p25`/`median`/`p75`, `jarque_bera`, `min`, `max`) with
  `annualized_return`/`annualized_vol`/`sharpe`, followed by a
  `[summary.ret_*.character]` read of plain-word labels
  (`trend`/`volatility`/`skew`/`tails`/`distribution`, and a `regime` from a
  `vol:N` column) from fixed thresholds. The return method follows the `ret:` spec.
  Annualization defaults to the data's own bar frequency (returns ÷ calendar-years,
  so daily/weekly/intraday are all correct); `--periods-per-year F` overrides it.
- **`plot`** renders OHLC candlesticks — with the same indicator specs as `calc`
  (price overlays, a volume panel via `volume`/`vsma`/`vema`/`vdema`, and stacked
  panels for `rsi`/`ret`/`vol`) — as an inline Sixel image on the console, or a
  `.png` (or raw `.six`/`.sixel`) file with `--output`. Tick and bar files are
  both accepted and resampled to the requested interval.

`--use-rth` keeps only regular-trading-hours ticks. Sessions are defined in an
exchange timezone (DST-correct), defaulting to `09:30-16:00 America/New_York`
(extended hours default to `04:00-20:00`); override with `--session HH:MM-HH:MM`
and `--tz NAME`. That timezone also anchors day/week/month/year bars even when
`--use-rth` is off, so such a bar's timestamp is the start of the period in UTC
(e.g. ET midnight = `05:00:00Z` in winter). Restrict the time window with
`--start`/`--end` or a positional `START..END` token (either side optional, e.g.
`2024-01-01..2026-01-01` or `..2026-01-01`); a bare date or date-time is read in
the exchange timezone (add `Z` or a `±HH` offset for an absolute instant), and a
bare end date includes the whole local day. `bars` and `calc`
render through fwob's formatter, so their output matches `fwob cat` of the
equivalent `.fwob`: `table`/`md` show fixed-point values and a UTC RFC3339
datetime (`time`); `csv`/`jsonl`/`raw` carry the exact stored integers (prices
×10⁴, epoch seconds), with absent calc values empty/`null`. With more than one
symbol the output gains a leading `Symbol` column.

The same TOML file used for downloads can carry an `[analysis]` section (see
`contracts.example.toml`); only that section is read by the analysis commands,
and CLI tokens/flags override it. A symbol universe under `[analysis].symbols`
is used when no positional path/symbol is given.

### Library API

The analysis engine is also a public library, so programs can read ticks,
resample, and run indicator pipelines directly — including custom user functions:

```rust
use mdfwob::analysis::{read_ticks, resample, BarClock, Calc, Interval, Sma, TickQuery};

let (symbol, ticks) = read_ticks("AAPL.fwob".as_ref(), &TickQuery::default())?;
let interval = Interval::parse("5m").unwrap()?;
let bars = resample(&ticks, interval, false, &BarClock::Utc);
let out = Calc::new(&bars)
    .with(Sma { period: 20 })
    .with_fn("zscore", |bars| {
        // any Fn(&[Bar]) -> Vec<Option<f64>>
        bars.iter().map(|b| Some(b.close)).collect()
    })
    .run();
# Ok::<(), anyhow::Error>(())
```

## Install

Install the command-line tool from crates.io:

```text
cargo install mdfwob
```

For an exactly reproducible build using the dependency versions pinned in the
published `Cargo.lock`, add `--locked`.

## Build

Rust 1.88 or newer is required.

```text
cargo build --release
```

The executable is written to `target/release/mdfwob` (`mdfwob.exe` on
Windows). IBKR downloads require a running TWS or IB Gateway session with API
access enabled. Databento downloads require the configured API-key environment
variable.

Copy `contracts.example.toml` to a local `contracts.toml` and adjust provider,
output, and contract settings. `contracts.toml`, downloaded FWOB files, logs,
and scratch directories are intentionally excluded from Git.

The repository ships a tracked pre-commit hook (`.githooks/pre-commit`) that runs
`cargo fmt --check`, matching the CI formatting gate so a style slip fails locally
instead of on a pushed commit. Enable it once per clone with:

```text
git config core.hooksPath .githooks
```

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

## Release

From a clean `main` branch that exactly matches `origin/main`, bump, validate,
commit, tag, and push a release with:

```powershell
.\scripts\release.ps1 patch
```

The accepted levels are `major`, `minor`, and `patch`. The script updates
`Cargo.toml` and `Cargo.lock`, runs formatting, clippy, tests, and the release
build, then atomically pushes the release commit and annotated `vX.Y.Z` tag.
