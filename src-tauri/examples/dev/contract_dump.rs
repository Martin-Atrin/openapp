//! Dump the agent-facing contract (describe JSON, MCP tool list with input schemas, host actions) for refactor
//! safety nets: run before and after a change and diff the output.
//! Run: `OPENAPP_STATE_HOME=<scratch> cargo run --example contract_dump -- <workspace> <out.json>`

use openapp_lib::kernel::{Kernel, NullSink};
use serde_json::{Value, json};
use std::sync::Arc;

/// Drop values that differ per run (fresh session ids, timestamps, the workspace path).
fn normalize(v: &mut Value, ws: &str) {
    match v {
        Value::String(s) if s.contains(ws) => *s = s.replace(ws, "<ws>"),
        Value::Array(a) => a.iter_mut().for_each(|x| normalize(x, ws)),
        Value::Object(o) => {
            for key in ["sessions", "activeSession", "created", "updated", "ts"] {
                if o.contains_key(key) {
                    o.insert(key.into(), json!("<volatile>"));
                }
            }
            o.values_mut().for_each(|x| normalize(x, ws));
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let ws = std::path::PathBuf::from(args.next().expect("workspace path"));
    let out = args.next().expect("output path");
    let k = Kernel::new_opts(ws, None, None, Arc::new(NullSink))?;
    k.start(false).await?;
    let mut tools = openapp_lib::mcp_server::tools();
    tools.extend(openapp_lib::primitives::mcp_tools());
    let tools: Vec<Value> = tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": *t.input_schema })).collect();
    let mut dump = json!({
        "describe": k.describe(),
        "mcp_tools": tools,
        "host_actions": openapp_lib::primitives::host_actions(),
        "kernel_methods": openapp_lib::primitives::connect::METHODS,
    });
    let ws = k.workspace.to_string_lossy().to_string();
    normalize(&mut dump, &ws);
    std::fs::write(&out, serde_json::to_string_pretty(&dump)?)?;
    eprintln!("wrote {out}");
    k.shutdown();
    Ok(())
}
