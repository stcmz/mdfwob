//! End-to-end tests for `mdfwob mcp`, driven over the real stdio transport.
//!
//! These spawn the actual binary and speak newline-delimited JSON-RPC to it, so they cover the
//! things a unit test cannot: that the handshake completes, that stdout carries *only* protocol
//! frames (a stray `println!` or a log line on stdout would desynchronize any client), and that
//! the values a model receives are in human units rather than raw storage units.

#![cfg(feature = "mcp")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use fwob::Writer;
use fwob_v2::WriterOptions;
use serde_json::{Value, json};

use mdfwob::tick::{Tick, tick_schema};

fn temp_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mdfwob-mcp-{tag}-{nonce}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `AAPL.fwob`: 20 one-minute ticks from 2024-01-02 09:30 New York, prices 185.0..186.9.
fn write_tick_file(dir: &Path) -> PathBuf {
    let path = dir.join("AAPL.fwob");
    let mut writer = Writer::create_v2(&path, tick_schema(), WriterOptions::new("AAPL"))
        .expect("create tick file");
    let base = 1_704_205_800u32; // 2024-01-02T14:30:00Z == 09:30 America/New_York
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

/// A live `mdfwob mcp` process plus its framed stdio.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Spawns the server rooted at `dir` and completes the MCP handshake.
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mdfwob"))
            .args(["mcp", "--root", dir.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mdfwob mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };

        let init = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "mdfwob-e2e", "version": "0" },
            }),
        );
        assert_eq!(
            init["serverInfo"]["name"], "mdfwob",
            "unexpected server identity: {init}"
        );
        assert!(
            init["instructions"]
                .as_str()
                .is_some_and(|s| s.contains("mdfwob_ls")),
            "instructions should point the model at ls first: {init}"
        );
        server.notify("notifications/initialized", json!({}));
        server
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write frame");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&message);
    }

    /// Sends a request and returns its `result`, panicking on a protocol-level error.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));

        // Skip any server-initiated traffic until our own response arrives.
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read frame");
            assert!(read > 0, "server closed stdout while awaiting {method}");
            // Every line on stdout must be a protocol frame; anything else means something
            // printed to stdout and corrupted the transport.
            let message: Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|e| {
                panic!("non-JSON-RPC line on stdout ({e}): {:?}", line.trim_end())
            });
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                panic!("{method} failed: {error}");
            }
            return message["result"].clone();
        }
    }

    /// Calls a tool, returning the whole `tools/call` result.
    fn call_raw(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// Calls a tool and returns its structured payload.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.call_raw(name, arguments);
        assert_ne!(
            result["isError"],
            json!(true),
            "{name} returned an error: {result}"
        );
        result["structuredContent"].clone()
    }

    /// Calls a tool expecting failure, returning the rendered error text.
    fn call_expecting_error(&mut self, name: &str, arguments: Value) -> String {
        let result = self.call_raw(name, arguments);
        assert_eq!(
            result["isError"],
            json!(true),
            "{name} unexpectedly succeeded: {result}"
        );
        result["content"].to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_lists_only_read_only_tools() {
    let dir = temp_dir("tools");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let listed = server.request("tools/list", json!({}));
    let tools = listed["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "mdfwob_bars",
            "mdfwob_calc",
            "mdfwob_inspect",
            "mdfwob_ls",
            "mdfwob_plot",
            "mdfwob_stat",
            "mdfwob_verify",
        ]
    );
    // Nothing that writes or downloads may ever appear here.
    assert!(
        !names
            .iter()
            .any(|n| n.contains("download") || n.contains("write")),
        "a mutating tool leaked into the surface: {names:?}"
    );
    // The typed schema is the whole point: parameters must not be a token bag.
    let bars = tools.iter().find(|t| t["name"] == "mdfwob_bars").unwrap();
    let props = &bars["inputSchema"]["properties"];
    for field in ["symbols", "interval", "start", "end", "limit"] {
        assert!(!props[field].is_null(), "{field} missing from bars schema");
    }

    // MCP requires both schemas to describe an *object*. A tool that advertises a top-level array
    // is rejected wholesale by a validating client — Claude Code refuses the entire tool list with
    // `expected "object" (at tools.N.outputSchema.type)`, so every tool disappears, not just the
    // offending one. Returning `Vec<T>` from a tool is the easy way to reintroduce that.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{name} inputSchema must be an object schema"
        );
        // `outputSchema` is optional (a tool returning only content blocks has none), but when
        // present it must be an object schema.
        if !tool["outputSchema"].is_null() {
            assert_eq!(
                tool["outputSchema"]["type"], "object",
                "{name} outputSchema must be an object schema, not a bare array"
            );
        }
    }

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ls_reports_the_archive_in_human_units() {
    let dir = temp_dir("ls");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let rows = server.call("mdfwob_ls", json!({}));
    let rows = rows["items"].as_array().expect("ls rows");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["symbol"], "AAPL");
    assert_eq!(row["kind"], "tick");
    assert_eq!(row["frame_count"], 20);
    // Relative to the root — the host's absolute layout must not leak.
    assert_eq!(row["file"], "AAPL.fwob");
    // Exchange-local ISO-8601, not a bare epoch and not UTC.
    let first = row["first"].as_str().expect("first timestamp");
    assert!(
        first.starts_with("2024-01-02T09:30:00") && first.ends_with("-05:00"),
        "expected an exchange-local timestamp, got {first}"
    );

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bars_returns_real_prices_and_local_times() {
    let dir = temp_dir("bars");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let series = server.call(
        "mdfwob_bars",
        json!({ "symbols": ["AAPL"], "interval": "5m" }),
    );
    let series = &series["items"].as_array().expect("series")[0];
    assert_eq!(series["symbol"], "AAPL");
    assert_eq!(series["interval"], "5m");
    assert_eq!(series["total"], 4);
    assert_eq!(series["truncated"], json!(false));

    let rows = series["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 4);
    let close = rows[0]["close"].as_f64().expect("close");
    // The frame formatter's `jsonl` would report this as 1_854_000 (a price scaled by 10^4).
    assert!(
        (184.0..187.0).contains(&close),
        "close {close} is not a real price"
    );
    let time = rows[0]["time"].as_str().expect("time");
    assert!(
        time.ends_with("-05:00"),
        "expected a numeric tz offset, got {time}"
    );

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn limit_truncates_to_the_most_recent_rows() {
    let dir = temp_dir("limit");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let full = server.call(
        "mdfwob_bars",
        json!({ "symbols": ["AAPL"], "interval": "5m" }),
    );
    let all_rows = full["items"].as_array().unwrap()[0]["rows"]
        .as_array()
        .unwrap()
        .clone();

    let capped = server.call(
        "mdfwob_bars",
        json!({ "symbols": ["AAPL"], "interval": "5m", "limit": 2 }),
    );
    let series = &capped["items"].as_array().expect("series")[0];
    assert_eq!(series["total"], 4);
    assert_eq!(series["returned"], 2);
    assert_eq!(series["truncated"], json!(true));
    assert!(
        series["hint"]
            .as_str()
            .is_some_and(|h| h.contains("narrow")),
        "a truncated result must say how to get a complete one: {series}"
    );

    // The retained rows are the trailing ones — a model asking about a long history means "and
    // what happened lately".
    let kept = series["rows"].as_array().expect("rows");
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0]["time"], all_rows[2]["time"]);
    assert_eq!(kept[1]["time"], all_rows[3]["time"]);

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn calc_warms_up_over_the_full_series_even_when_truncated() {
    let dir = temp_dir("calc");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let series = server.call(
        "mdfwob_calc",
        json!({
            "symbols": ["AAPL"],
            "interval": "5m",
            "specs": ["sma:2"],
            "limit": 2,
        }),
    );
    let series = &series["items"].as_array().expect("series")[0];
    assert_eq!(series["total"], 4);
    assert_eq!(series["returned"], 2);

    // Bars 3 and 4 are past sma:2's warm-up, so both carry values despite the earlier bars having
    // been dropped from the window — the indicator still saw them.
    for row in series["rows"].as_array().expect("rows") {
        assert!(
            row["values"]["sma_2"].as_f64().is_some(),
            "indicator state was lost under truncation: {row}"
        );
    }

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn stat_and_verify_agree_with_the_cli() {
    let dir = temp_dir("stat");
    let path = write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let rows = server.call("mdfwob_stat", json!({ "symbols": ["AAPL"] }));
    let row = &rows["items"].as_array().expect("stat rows")[0];
    assert_eq!(row["symbol"], "AAPL");
    assert_eq!(row["trades"], 20);
    assert_eq!(row["volume"], 2_000);
    let min = row["min"].as_f64().expect("min");
    let max = row["max"].as_f64().expect("max");
    assert!((min - 185.0).abs() < 1e-4, "min was {min}");
    assert!((max - 186.9).abs() < 1e-4, "max was {max}");

    // The CLI is the reference implementation; the two must not drift.
    let cli = Command::new(env!("CARGO_BIN_EXE_mdfwob"))
        .args(["stat", path.to_str().unwrap(), "jsonl"])
        .output()
        .expect("run mdfwob stat");
    assert!(cli.status.success(), "cli stat failed");
    let cli_row: Value =
        serde_json::from_str(String::from_utf8_lossy(&cli.stdout).lines().next().unwrap())
            .expect("cli jsonl");
    assert_eq!(cli_row["trades"], row["trades"]);
    assert_eq!(cli_row["volume"], row["volume"]);

    let verified = server.call("mdfwob_verify", json!({ "symbol": "AAPL" }));
    assert_eq!(verified["status"], "ok");
    assert_eq!(verified["kind"], "tick");
    assert_eq!(verified["frame_count"], 20);
    assert_eq!(verified["data"]["trades"], 20);

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn inspect_describes_the_schema_without_scanning() {
    let dir = temp_dir("inspect");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let report = server.call("mdfwob_inspect", json!({ "symbol": "AAPL" }));
    assert_eq!(report["symbol"], "AAPL");
    assert_eq!(report["kind"], "tick");
    assert_eq!(report["format"], "fwob-v2");
    assert_eq!(report["frame_count"], 20);
    assert!(
        report["granularity"].is_null(),
        "tick files have no interval"
    );
    let fields = report["fields"].as_array().expect("schema fields");
    assert!(
        fields.iter().any(|f| f["name"] == "time"),
        "schema should list the time column: {fields:?}"
    );
    assert!(
        report["preview"].as_str().is_some_and(|p| !p.is_empty()),
        "expected a decoded preview"
    );

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn plot_returns_a_png_image_block() {
    let dir = temp_dir("plot");
    write_tick_file(&dir);
    let mut server = Server::start(&dir);

    let result = server.call_raw(
        "mdfwob_plot",
        json!({ "symbols": ["AAPL"], "interval": "5m", "width": 640, "height": 400 }),
    );
    assert_ne!(result["isError"], json!(true), "plot failed: {result}");
    let content = result["content"].as_array().expect("content blocks");
    let image = content
        .iter()
        .find(|block| block["type"] == "image")
        .unwrap_or_else(|| panic!("no image block in {content:?}"));
    assert_eq!(image["mimeType"], "image/png");

    let encoded = image["data"].as_str().expect("base64 data");
    let bytes = base64_decode(encoded);
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "payload is not a PNG (first bytes: {:?})",
        &bytes[..8.min(bytes.len())]
    );

    drop(server);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn paths_outside_the_root_are_rejected() {
    let dir = temp_dir("escape");
    write_tick_file(&dir);
    // A file the server must not reach, one level above its root.
    let outside = dir.parent().unwrap().join("mdfwob-mcp-outside.fwob");
    fs::write(&outside, b"not yours").unwrap();
    let mut server = Server::start(&dir);

    let error = server.call_expecting_error(
        "mdfwob_inspect",
        json!({ "symbol": "../mdfwob-mcp-outside" }),
    );
    assert!(
        error.contains("escapes the server root"),
        "unexpected error: {error}"
    );

    let absolute = outside.to_str().unwrap();
    let error = server.call_expecting_error("mdfwob_inspect", json!({ "symbol": absolute }));
    assert!(error.contains("absolute path"), "unexpected error: {error}");

    drop(server);
    let _ = fs::remove_file(outside);
    let _ = fs::remove_dir_all(dir);
}

/// Minimal standard-alphabet base64 decoder, so the test asserts on the wire bytes rather than
/// trusting the same encoder the server used.
fn base64_decode(input: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = TABLE
            .iter()
            .position(|&c| c == byte)
            .unwrap_or_else(|| panic!("invalid base64 byte {byte:?}"));
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
