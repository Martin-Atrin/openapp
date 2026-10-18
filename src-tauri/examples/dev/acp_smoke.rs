//! Real-harness ACP smoke test (no Tauri UI).
//! Run: `cargo run --example acp_smoke -- [harness-id] [prompt]` (default: claude).
//! Permission prompts are auto-approved with the first allow option.

#[path = "../eval/agent_run.rs"]
mod agent_run;

use openapp_lib::kernel::Kernel;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ws: PathBuf = std::env::var_os("OPENAPP_SMOKE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("openapp-acp-smoke"));
    let id = std::env::args().nth(1).unwrap_or_else(|| "claude".into());
    let prompt = std::env::args().nth(2).unwrap_or_else(|| "Call openapp_describe and reply with the workspace path only.".into());
    let k = Kernel::new_opts(ws, openapp_lib::harness::custom_from_env(), Some(&id), Arc::new(agent_run::Print::default()))?;
    println!("workspace: {} · harness: {:?}", k.workspace.display(), openapp_lib::harness::selected(&k).map(|s| s.cmd));
    k.start(true).await?;
    match agent_run::wait_ready(&k, Duration::from_secs(400)).await {
        Ok(t) => println!("agent ready after {t:?}"),
        Err(e) => {
            println!("AGENT ERROR: {e}");
            std::process::exit(2);
        }
    }
    let r = agent_run::run_prompt(&k, &prompt, Duration::from_secs(600)).await;
    println!("turn: {r:?}");
    println!("final agent text: {}", agent_run::last_agent_text(&k));
    k.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}
