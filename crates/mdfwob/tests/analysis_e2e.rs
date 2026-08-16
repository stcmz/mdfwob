//! End-to-end tests for the analysis engine and CLI subcommands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use fwob::Writer;
use fwob_v2::WriterOptions;

use mdfwob::analysis::config::ReturnMethod;
use mdfwob::analysis::output::{AnalysisFormat, BarSeries, write_bars};
use mdfwob::analysis::{
    BarClock, Calc, Interval, Sma, TickQuery, compute_stat, read_bars, read_ticks, resample,
    summarize,
};
use mdfwob::tick::{Tick, tick_schema};

fn temp_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdfwob-e2e-{tag}-{nonce}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes an `AAPL.fwob` tick file: one tick per minute, ascending prices.
fn write_tick_file(dir: &Path) -> PathBuf {
    let path = dir.join("AAPL.fwob");
    let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new("AAPL"))
        .expect("create tick file");
    // 2024-01-02 14:30:00Z, then every 60s. 20 ticks => 14:30..14:49.
    let base = 1_704_205_800u32;
    let mut buf = Vec::new();
    for i in 0..20u32 {
        let price = 185.0 + f64::from(i) * 0.1;
        Tick::new(base + i * 60, price, 100)
            .unwrap()
            .encode(&mut buf);
    }
    writer.append_presorted_frames(&buf).unwrap();
    writer.finish().unwrap();
    path
}

#[test]
fn library_api_reads_resamples_and_computes() {
    let dir = temp_dir("lib");
    let path = write_tick_file(&dir);

    let (symbol, ticks) = read_ticks(&path, &TickQuery::default()).unwrap();
    assert_eq!(symbol, "AAPL");
    assert_eq!(ticks.len(), 20);

    // 5m buckets over 60s ticks => 5 ticks per bar, 4 bars.
    let interval = Interval::parse("5m").unwrap().unwrap();
    let bars = resample(&ticks, interval, false, &BarClock::Utc);
    assert_eq!(bars.len(), 4);
    assert_eq!(bars[0].trades, 5);

    let row = compute_stat(symbol, "fwob-v2".into(), &ticks);
    assert_eq!(row.trades, 20);
    assert_eq!(row.volume, 2_000);
    assert_eq!(row.kind, "tick");

    // calc: built-in + custom function over the bars.
    let out = Calc::new(&bars)
        .with(Sma { period: 2 })
        .with_fn("close_plus_one", |bars| {
            bars.iter().map(|b| Some(b.close + 1.0)).collect()
        })
        .run();
    assert_eq!(out.columns.len(), 2);
    assert_eq!(out.columns[0].name, "sma_2");
    assert_eq!(out.columns[1].name, "close_plus_one");
    assert_eq!(out.columns[0].values[0], None); // warm-up
    assert!(out.columns[0].values[1].is_some());

    let summary = summarize(&bars, ReturnMethod::Log, true, 252.0).unwrap();
    assert_eq!(summary.count, 3);
    assert!(summary.annualized.is_some());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bars_fwob_round_trips_through_library() {
    let dir = temp_dir("barsfwob");
    let path = write_tick_file(&dir);
    let (symbol, ticks) = read_ticks(&path, &TickQuery::default()).unwrap();
    let interval = Interval::parse("5m").unwrap().unwrap();
    let bars = resample(&ticks, interval, false, &BarClock::Utc);

    let out_dir = dir.join("bars");
    let series = vec![BarSeries {
        symbol: symbol.clone(),
        bars: bars.clone(),
    }];
    let mut sink = Vec::new();
    write_bars(
        &series,
        AnalysisFormat::Fwob,
        Some(out_dir.as_path()),
        &mut sink,
    )
    .unwrap();

    let bar_path = out_dir.join("AAPL.fwob");
    assert!(bar_path.exists());
    let (bar_symbol, read_back) = read_bars(&bar_path).unwrap();
    assert_eq!(bar_symbol, "AAPL");
    assert_eq!(read_back.len(), bars.len());
    assert!((read_back[0].close - bars[0].close).abs() < 1e-4);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_stat_bars_calc_run() {
    let dir = temp_dir("cli");
    let path = write_tick_file(&dir);
    let path_str = path.to_str().unwrap();
    let exe = env!("CARGO_BIN_EXE_mdfwob");

    // stat (default table)
    let out = Command::new(exe).args(["stat", path_str]).output().unwrap();
    assert!(
        out.status.success(),
        "stat failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("symbol"));
    assert!(stdout.contains("AAPL"));

    // bars 5m csv: headers are the schema field names; values are raw stored integers
    // (price * 10_000, epoch time) -- identical to `fwob cat ... csv`.
    let out = Command::new(exe)
        .args(["bars", path_str, "5m", "csv"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bars failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("time,open,high,low,close,volume,vwap,trades"),
        "{stdout}"
    );
    // First 5m bar opens at 185.0 -> stored 1_850_000; time is the raw epoch second.
    assert!(stdout.contains("1704205800,1850000,"), "{stdout}");

    // bars 5m table: time renders as RFC3339 (unified with fwob cat).
    let out = Command::new(exe)
        .args(["bars", path_str, "5m"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("time"), "{stdout}");
    assert!(stdout.contains("2024-01-02T14:30:00Z"), "{stdout}");

    // calc 5m sma:2 rsi:3 (table) with summary
    let out = Command::new(exe)
        .args(["calc", path_str, "5m", "sma:2", "ret:log", "--summary"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "calc failed ({}):\nSTDOUT:\n{}\nSTDERR:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sma_2"));
    assert!(stdout.contains("ret_log"));
    // The summary footer is a colored-TOML block (plain here since stdout is piped): a `[summary]`
    // table, a `[summary.price]` block (drawdown/CAGR), a per-indicator `[summary.sma_2]` block,
    // and a `[summary.ret_log]` fitted-normal block with annualized figures and a `.character`
    // sub-block, all driven by the ret:log column.
    assert!(stdout.contains("[summary]"), "{stdout}");
    assert!(stdout.contains("[summary.price]"), "{stdout}");
    assert!(stdout.contains("[summary.sma_2]"), "{stdout}");
    assert!(stdout.contains("[summary.ret_log]"), "{stdout}");
    assert!(stdout.contains("skew = "), "{stdout}");
    assert!(stdout.contains("annualized_return = "), "{stdout}");
    assert!(stdout.contains("sharpe = "), "{stdout}");
    assert!(stdout.contains("[summary.ret_log.character]"), "{stdout}");
    assert!(stdout.contains("trend = "), "{stdout}");

    // calc with NO interval token defaults to 1d (like bars/plot) instead of erroring on a tick
    // file. The 20-minute tick span collapses into a single 1d bar.
    let out = Command::new(exe)
        .args(["calc", path_str, "sma:2"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "calc without an interval token should default to 1d, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("sma_2"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_plot_accepts_tick_and_bar_files() {
    let dir = temp_dir("cliplot");
    let tick = write_tick_file(&dir);
    let exe = env!("CARGO_BIN_EXE_mdfwob");

    let assert_png = |path: &Path| {
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() > 8, "png {} too small", path.display());
        assert_eq!(&bytes[0..4], b"\x89PNG", "{} is not a PNG", path.display());
    };

    // Plot the tick file (resampled to 5m) to a PNG.
    let tick_png = dir.join("tick.png");
    let out = Command::new(exe)
        .args([
            "plot",
            tick.to_str().unwrap(),
            "5m",
            "rsi:3",
            "volume",
            "-o",
            tick_png.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plot tick failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_png(&tick_png);

    // Produce a pre-aggregated bar file, then plot it directly (no interval token needed).
    let bar_dir = dir.join("bars");
    let out = Command::new(exe)
        .args([
            "bars",
            tick.to_str().unwrap(),
            "5m",
            "fwob",
            "--output",
            bar_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bars fwob failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bar_file = bar_dir.join("AAPL.fwob");
    assert!(bar_file.exists(), "bar file not written");

    let bar_png = dir.join("bar.png");
    let out = Command::new(exe)
        .args([
            "plot",
            bar_file.to_str().unwrap(),
            "-o",
            bar_png.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "plot bar file failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_png(&bar_png);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_inspect_and_verify() {
    let dir = temp_dir("inspectverify");
    let tick = write_tick_file(&dir); // AAPL: 20 one-minute ticks, 09:30..09:49 ET (all RTH)
    let tick_str = tick.to_str().unwrap();
    let exe = env!("CARGO_BIN_EXE_mdfwob");

    // inspect (piped => plain, valid TOML). Timestamps honor the exchange tz (winter ET = -05:00).
    let out = Command::new(exe)
        .args(["inspect", tick_str])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("[file]"), "{s}");
    assert!(s.contains("kind = \"tick\""), "{s}");
    assert!(s.contains("frame_type = \"Tick\""), "{s}");
    assert!(s.contains("[range]"), "{s}");
    assert!(s.contains("timezone = \"America/New_York\""), "{s}");
    assert!(s.contains("-05:00"), "expected tz offset, not UTC: {s}");
    assert!(s.contains("hours = \"rth\""), "{s}");
    assert!(s.contains("[schema]"), "{s}");
    assert!(s.contains("[frames]"), "{s}");
    // Pruned storage-layer sections must be absent.
    assert!(!s.contains("[compression]"), "{s}");
    assert!(!s.contains("[pages]"), "{s}");
    // A tick file has no bar granularity.
    assert!(!s.contains("granularity"), "{s}");

    // --tz UTC renders +00:00 offsets.
    let out = Command::new(exe)
        .args(["inspect", "--tz", "UTC", tick_str])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("timezone = \"UTC\""), "{s}");
    assert!(s.contains("+00:00"), "{s}");

    // verify: structural + identity + a scanned [data] block.
    let out = Command::new(exe)
        .args(["verify", tick_str])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("[verify]"), "{s}");
    assert!(s.contains("status = \"ok\""), "{s}");
    assert!(s.contains("kind = \"tick\""), "{s}");
    assert!(s.contains("frame_count = 20"), "{s}");
    assert!(s.contains("[data]"), "{s}");
    assert!(s.contains("trades = 20"), "{s}");
    assert!(s.contains("min = 185.0000"), "{s}");
    assert!(s.contains("vwap = "), "{s}");

    // Produce a bar file and inspect it → kind = bar, with a detected 5m granularity.
    let bar_dir = dir.join("bars");
    let out = Command::new(exe)
        .args([
            "bars",
            tick_str,
            "5m",
            "fwob",
            "--output",
            bar_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "bars fwob failed");
    let bar_file = bar_dir.join("AAPL.fwob");
    let out = Command::new(exe)
        .args(["inspect", bar_file.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("kind = \"bar\""), "{s}");
    assert!(s.contains("granularity = \"5m\""), "{s}");

    // Strict: a Calc-schema file (not Tick/Bar) is rejected by both commands.
    let calc_dir = dir.join("calc");
    let out = Command::new(exe)
        .args([
            "calc",
            bar_file.to_str().unwrap(),
            "sma:2",
            "fwob",
            "--output",
            calc_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "calc fwob failed");
    let calc_file = calc_dir.join("AAPL.fwob");
    let cf = calc_file.to_str().unwrap();
    for cmd in ["inspect", "verify"] {
        let out = Command::new(exe)
            .args([cmd, cf])
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{cmd} should reject a Calc file");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(combined.contains("Calc"), "{cmd} output: {combined}");
    }

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cli_ls_lists_tick_and_bar_files() {
    let dir = temp_dir("cli-ls");
    write_tick_file(&dir); // AAPL.fwob tick file (09:30..09:49 ET, all RTH)
    let exe = env!("CARGO_BIN_EXE_mdfwob");

    // Add a bar file alongside it so the listing has two rows of differing kind.
    let bar_dir = dir.join("bars");
    let out = Command::new(exe)
        .args([
            "bars",
            dir.join("AAPL.fwob").to_str().unwrap(),
            "5m",
            "fwob",
            "--output",
            bar_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "bars fwob failed");

    // ls the tick directory (table): one row, kind = tick, tz-aware time, hours column.
    let out = Command::new(exe)
        .args(["ls", dir.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("file"), "{s}"); // header
    assert!(s.contains("granularity"), "{s}");
    assert!(s.contains("AAPL"), "{s}");
    assert!(s.contains("tick"), "{s}");
    assert!(s.contains("-05:00"), "expected tz-aware time: {s}"); // winter ET

    // ls the bar dir as CSV: kind = bar, a detected 5m granularity.
    let out = Command::new(exe)
        .args(["ls", bar_dir.to_str().unwrap(), "csv"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("file,symbol,kind,format,frames"), "{s}");
    assert!(s.contains(",bar,"), "{s}");
    assert!(s.contains(",5m,"), "{s}");

    let _ = fs::remove_dir_all(dir);
}

/// `unsync` derives what to delete from live tick sources, so a file that merely *looks* like a
/// sidecar is never touched. Deleting by filename pattern would remove a leftover whose symbol no
/// longer exists — data the user never asked to lose.
#[test]
fn cli_unsync_removes_only_sidecars_backed_by_a_live_source() {
    let dir = temp_dir("unsync");
    let path = write_tick_file(&dir);
    let exe = env!("CARGO_BIN_EXE_mdfwob");
    let dir_str = dir.to_str().unwrap();

    let out = Command::new(exe)
        .args(["sync", dir_str, "5m", "rth"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Progress goes to stderr so stdout stays a pipeable summary.
    let progress = String::from_utf8_lossy(&out.stderr);
    assert!(progress.contains("AAPL"), "no progress line: {progress}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("symbol"),
        "summary should be on stdout"
    );

    let sidecar = dir.join("AAPL.5m.rth.bars.fwob");
    assert!(sidecar.exists(), "sync should have built it");

    // A leftover from a symbol whose tick file is gone.
    let orphan = dir.join("GONE.5m.rth.bars.fwob");
    fs::copy(&sidecar, &orphan).unwrap();

    let out = Command::new(exe)
        .args(["unsync", dir_str, "5m", "rth", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "unsync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("AAPL"), "should list what it removes");
    assert!(
        !stdout.contains("GONE"),
        "an orphan must not even be listed: {stdout}"
    );

    assert!(!sidecar.exists(), "the backed sidecar should be gone");
    assert!(orphan.exists(), "the orphan must survive");
    assert!(path.exists(), "the tick source is never touched");

    // Nothing left to do is a clean no-op, not an error.
    let out = Command::new(exe)
        .args(["unsync", dir_str, "5m", "rth", "--yes"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no 5m rth sidecars"));

    let _ = fs::remove_dir_all(dir);
}

/// The `rth` token must survive a round trip through `sync`.
///
/// A daily sidecar carries its bucket-start timestamp — local midnight — which sits outside every
/// intraday session window. Applying the session as a *row* filter to it therefore discards the
/// whole file, and the commands returned an empty table rather than an error. The trap is that it
/// fails selectively: a 1m sidecar survives, because those timestamps do fall inside the session.
///
/// So: build both sidecars from the same ticks, then read each back with `rth` and require the
/// rows to appear — while the tick source still filters, which is what the filter is for.
#[test]
fn cli_reads_a_synced_sidecar_back_with_the_rth_token() {
    let dir = temp_dir("cli-rth-bars");
    write_tick_file(&dir); // AAPL.fwob, 14:30..14:49Z = 09:30..09:49 ET, all in RTH
    let exe = env!("CARGO_BIN_EXE_mdfwob");
    let dir_str = dir.to_str().unwrap();

    for interval in ["1d", "1m"] {
        let out = Command::new(exe)
            .args(["sync", dir_str, interval, "rth"])
            .output()
            .unwrap();
        assert!(out.status.success(), "sync {interval} failed");

        let sidecar = dir.join(format!("AAPL.{interval}.rth.bars.fwob"));
        assert!(sidecar.exists(), "sync did not write {}", sidecar.display());
        let sidecar_str = sidecar.to_str().unwrap();

        // Reading that file back, with the very token it was built under, must return its rows.
        for args in [
            vec!["bars", sidecar_str, interval, "rth"],
            vec!["calc", sidecar_str, interval, "rth", "sma:2"],
        ] {
            let out = Command::new(exe).args(&args).output().unwrap();
            assert!(out.status.success(), "{args:?} failed");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                text.lines().count() > 1,
                "{args:?} returned only a header:\n{text}"
            );
        }

        let out = Command::new(exe)
            .args(["stat", sidecar_str, "rth"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.contains(" 0 "),
            "stat {interval} rth reported nothing:\n{text}"
        );
    }

    // The filter still has to do its job on the source it was written for. These ticks are all
    // inside regular hours, so an out-of-hours window must come back empty from the tick file.
    let ticks = dir.join("AAPL.fwob");
    let out = Command::new(exe)
        .args([
            "bars",
            ticks.to_str().unwrap(),
            "1d",
            "rth",
            "--session",
            "20:00-23:00",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().count(),
        1,
        "a session filter must still exclude ticks outside it"
    );

    let _ = fs::remove_dir_all(dir);
}
