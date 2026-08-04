//! `mdfwob mcp`: a read-only [Model Context Protocol](https://modelcontextprotocol.io) server.
//!
//! A second front end onto the same analysis engine the CLI drives, aimed at LLM callers rather
//! than a terminal. It speaks JSON-RPC over stdin/stdout, so an MCP client spawns it as a child
//! process and owns its lifetime — there is no port, no daemon, and no shared state between
//! clients:
//!
//! ```json
//! { "mcpServers": { "mdfwob": { "command": "mdfwob", "args": ["mcp", "--root", "DIR"] } } }
//! ```
//!
//! The server is read-only by construction: it exposes no write path, no downloader, and confines
//! every file it opens to `--root`.

mod dto;
mod root;
mod server;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use crate::cli::load_analysis_config;
use root::Root;
use server::McpServer;

#[derive(Debug, Args)]
#[command(
    after_help = "Register with an MCP client, e.g. Claude Code or Claude Desktop:

  { \"mcpServers\": { \"mdfwob\": { \"command\": \"mdfwob\", \"args\": [\"mcp\"] } } }

The server reads JSON-RPC on stdin and writes it on stdout; logs go to stderr. Every path it can \
reach is confined to --root, and it exposes only read-only tools (no download, no file writes)."
)]
pub struct McpArgs {
    /// Directory the server may read. Defaults to the config's output_dir, else the current
    /// directory. Symbols resolve to `<ROOT>/<SYMBOL>.fwob`.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Optional CONFIG.toml supplying analysis defaults (output_dir, session, timezone).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Default session window (HH:MM-HH:MM) for tools that do not override it.
    #[arg(long)]
    session: Option<String>,
    /// Default exchange timezone (IANA name). Default America/New_York.
    #[arg(long)]
    tz: Option<String>,
}

impl McpArgs {
    pub fn run(self) -> Result<()> {
        let acfg = load_analysis_config(self.config.as_deref())?;
        let root = Root::new(self.root, &acfg)?;
        let server = McpServer::new(root.clone(), acfg, self.session, self.tz);

        // stdout is the transport, so every diagnostic goes to stderr. `tracing` is already
        // configured that way in main; this is a reminder that anything printed to stdout here
        // would corrupt the JSON-RPC stream.
        tracing::info!(root = %root.dir().display(), "serving MCP on stdio");

        // The analysis engine is entirely blocking, so the runtime exists only to drive the
        // transport and the `spawn_blocking` pool. Built here rather than via `#[tokio::main]` so
        // `main` stays synchronous, matching the Databento provider's approach.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start the async runtime")?;
        runtime.block_on(async move {
            let service = server
                .serve(stdio())
                .await
                .context("failed to start the MCP server")?;
            service.waiting().await.context("MCP server failed")?;
            Ok(())
        })
    }
}
