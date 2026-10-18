//! Start the kernel headless and keep the MCP server up, for testing external MCP clients.
//! Writes `<workspace>/.mcp.json` (Claude Code project config) with the URL + bearer token.
//! Run: `cargo run --example mcp_serve -- <workspace> <seconds>`

use openapp_lib::kernel::{Kernel, NullSink};
use std::sync::Arc;

struct StderrLog;
impl log::Log for StderrLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("openapp")
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::set_logger(&StderrLog).ok();
    log::set_max_level(log::LevelFilter::Debug);
    let mut args = std::env::args().skip(1);
    let ws = std::path::PathBuf::from(args.next().expect("workspace path"));
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let k = Kernel::new_opts(ws, None, None, Arc::new(NullSink))?;
    k.start(false).await?;
    let ep = k.mcp.get().unwrap();
    let cfg = serde_json::json!({ "mcpServers": { "openapp": {
        "type": "http", "url": ep.url, "headers": { "Authorization": format!("Bearer {}", ep.token) } } } });
    std::fs::write(k.workspace.join(".mcp.json"), serde_json::to_string_pretty(&cfg)?)?;
    eprintln!("serving {} for {secs}s", ep.url);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    k.shutdown();
    Ok(())
}
