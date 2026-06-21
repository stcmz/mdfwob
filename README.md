# mdfwob

[![CI](https://github.com/stcmz/mdfwob/actions/workflows/ci.yml/badge.svg)](https://github.com/stcmz/mdfwob/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/mdfwob.svg)](https://crates.io/crates/mdfwob)

Standalone CLI for downloading historical market data into FWOB files.

The default output schema matches the legacy `ShortTick` layout:

```text
Time   u32  Unix seconds since the UTC epoch
Price  u32  price * 10,000
Size   i32  shares
```

`Time` is always an absolute UTC epoch second. TWS login timezone, computer
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
When an existing v2 file is resumed without writer-tuning tokens, FWOB 1.6
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
complete schema. Historical request starts are globally spaced by 3 seconds by
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
positive number for a wall-clock retry budget in seconds. Retries share the
global request pacer configured by `download.request_interval_ms`.

A TWS/IB Gateway upstream-connectivity blip (IBKR system codes 1100 then
1101/1102) can silently orphan an in-flight request without dropping the API
socket. A background watcher on the IBKR notice stream detects the restore and
re-issues the affected request from the unchanged cursor within about a second,
so the download resumes on its own without a gateway restart. A request that is
merely slow (no connectivity blip) is never re-issued. Ctrl+C is honored
promptly even while a request is blocked or stalled; a second Ctrl+C forces an
immediate exit.

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
