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
    assert!(stdout.contains("# summary:"));

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
