//! Module sources and kernel setups shared by the host and VM halves of the channels and wave-2 cases.

use super::{Collect, auto_answer, first_option, write_module};
use openapp_lib::kernel::Kernel;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// §21 G1/G9: short idle stop and resource sampling.
pub const CHANNELS_ENV: &[(&str, &str)] = &[("OPENAPP_IDLE_SECS", "3"), ("OPENAPP_SAMPLE_MS", "500")];

/// Minimal MCP stdio server without the SDK (works on the host and in the VM with plain node).
pub const CHANNELS_SERVER_TS: &str = r#"import { createInterface } from "node:readline";
const MOD = process.env.OPENAPP_MODULE;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function connect() {
  return new Promise((res, rej) => {
    const ws = new WebSocket(process.env.OPENAPP_CHANNEL_URL);
    ws.onopen = () => res(ws);
    ws.onerror = () => rej(new Error("channel websocket failed"));
  });
}
// view input → module: echo pub/in on pub/echo
if (process.env.ECHO === "1") {
  connect().then((ws) => {
    ws.onmessage = (e) => {
      const m = JSON.parse(String(e.data));
      if (m.op === "message" && m.topic === `${MOD}/in`) ws.send(JSON.stringify({ op: "publish", topic: `${MOD}/echo`, data: { echo: m.data } }));
    };
    ws.send(JSON.stringify({ op: "subscribe", topic: `${MOD}/in`, replay: false }));
  }).catch((e) => console.error(String(e)));
}
const tools = {
  ping: async () => `pong from ${process.platform}`,
  burst: async ({ n = 1000, topic }) => {
    const ws = await connect();
    const t0 = Date.now();
    for (let i = 0; i < n; i++) ws.send(JSON.stringify({ op: "publish", topic, data: { i } }));
    await sleep(300);
    ws.close();
    return `sent ${n} in ${Date.now() - t0} ms`;
  },
  replay: async ({ topic }) => {
    const ws = await connect();
    let count = 0, last = null;
    ws.onmessage = (e) => { const m = JSON.parse(String(e.data)); if (m.op === "message") { count++; last = m.data; } };
    ws.send(JSON.stringify({ op: "subscribe", topic }));
    await sleep(700);
    ws.close();
    return JSON.stringify({ count, last });
  },
  deny: async ({ topic }) => {
    const ws = await connect();
    const err = new Promise((res) => { ws.onmessage = (e) => { const m = JSON.parse(String(e.data)); if (m.op === "error") res(m.error); }; });
    ws.send(JSON.stringify({ op: "publish", topic, data: 1 }));
    const r = await Promise.race([err, sleep(2000).then(() => "no error")]);
    ws.close();
    return r;
  },
  post: async ({ topic, data }) => {
    const r = await fetch(`${process.env.OPENAPP_HOST_URL}/channel.publish`, {
      method: "POST",
      headers: { Authorization: `Bearer ${process.env.OPENAPP_HOST_TOKEN}`, "Content-Type": "application/json" },
      body: JSON.stringify({ topic, data }),
    });
    return `${r.status} ${await r.text()}`;
  },
};
const send = (m) => process.stdout.write(JSON.stringify(m) + "\n");
createInterface({ input: process.stdin }).on("line", async (line) => {
  let m;
  try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined) return;
  if (m.method === "initialize") return send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "raw", version: "1" } } });
  if (m.method === "tools/list") return send({ jsonrpc: "2.0", id: m.id, result: { tools: Object.keys(tools).map((name) => ({ name, description: name, inputSchema: { type: "object" } })) } });
  if (m.method === "ping") return send({ jsonrpc: "2.0", id: m.id, result: {} });
  if (m.method === "tools/call") {
    try {
      const text = await tools[m.params.name](m.params.arguments ?? {});
      send({ jsonrpc: "2.0", id: m.id, result: { content: [{ type: "text", text: String(text) }] } });
    } catch (e) {
      send({ jsonrpc: "2.0", id: m.id, result: { content: [{ type: "text", text: String(e) }], isError: true } });
    }
    return;
  }
  send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "method not found" } });
});
"#;

/// The channels workspace (a lazy module and a module with a visible `$stream` view), a kernel that allows
/// every consent card, started.
pub async fn channels_kernel(ws: &Path) -> anyhow::Result<(Arc<Kernel>, Arc<Collect>)> {
    println!("workspace {}  state {}  vm home {}", ws.display(), openapp_lib::state::state_dir(ws).display(), openapp_lib::vm::vm_home().display());
    // modules on disk before start (autoload decides eager vs idle)
    write_module(ws, "lazy", json!({ "id": "lazy", "name": "Lazy", "server": { "command": ["node", "server.ts"] },
        "views": { "panel": { "kind": "spec", "file": "views/panel.json" } } }),
        &[("server.ts", CHANNELS_SERVER_TS), ("views/panel.json", r#"{ "type": "button", "label": "Ping", "action": { "tool": "ping" } }"#)])?;
    write_module(ws, "viewmod", json!({ "id": "viewmod", "name": "View module", "server": { "command": ["node", "server.ts"] },
        "views": { "status": { "kind": "spec", "file": "views/status.json" } },
        "placements": [ { "view": "status", "anchor": "composer.above" } ] }),
        &[("server.ts", CHANNELS_SERVER_TS), ("views/status.json", r#"{ "type": "stack", "children": [ { "type": "text", "value": { "$stream": "viewmod/counter", "path": "n", "default": "no count" } } ] }"#)])?;
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.to_path_buf(), None, None, sink.clone())?;
    auto_answer(&k, &sink, |_| Some("allow".into()));
    k.start(false).await?;
    Ok((k, sink))
}

/// §21 G6: a fast job ticker.
pub const WAVE2_ENV: &[(&str, &str)] = &[("OPENAPP_JOBS_TICK_MS", "200")];

/// Minimal MCP stdio server (host and VM, plain node).
pub const WAVE2_SERVER_TS: &str = r#"import { createInterface } from "node:readline";
import { readFileSync, statSync } from "node:fs";
let ticks = 0;
const tools = {
  tick: async () => `tick ${++ticks}`,
  echo: async (a) => JSON.stringify(a.input ?? a),
  add: async ({ a, b }) => String(Number(a) + Number(b)),
  secret: async () => "the secret tool ran",
  env: async ({ name }) => process.env[name] ?? "(unset)",
  readfile: async ({ rel }) => {
    const p = `${process.env.OPENAPP_SECRETS_DIR ?? "/nonexistent"}/${rel}`;
    try { return JSON.stringify({ text: readFileSync(p, "utf8"), mode: (statSync(p).mode & 0o777).toString(8), path: p }); } catch (e) { return JSON.stringify({ error: String(e) }); }
  },
};
const schemas = { add: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } }, required: ["a", "b"] } };
const desc = { add: "Add two numbers a and b; returns the sum", secret: "A tool the agent must not use" };
const send = (m) => process.stdout.write(JSON.stringify(m) + "\n");
createInterface({ input: process.stdin }).on("line", async (line) => {
  let m;
  try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined) return;
  if (m.method === "initialize") return send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "raw", version: "1" } } });
  if (m.method === "tools/list") return send({ jsonrpc: "2.0", id: m.id, result: { tools: Object.keys(tools).map((name) => ({ name, description: desc[name] ?? name, inputSchema: schemas[name] ?? { type: "object" } })) } });
  if (m.method === "ping") return send({ jsonrpc: "2.0", id: m.id, result: {} });
  if (m.method === "tools/call") {
    try {
      const text = await tools[m.params.name](m.params.arguments ?? {});
      send({ jsonrpc: "2.0", id: m.id, result: { content: [{ type: "text", text: String(text) }] } });
    } catch (e) {
      send({ jsonrpc: "2.0", id: m.id, result: { content: [{ type: "text", text: String(e) }], isError: true } });
    }
    return;
  }
  send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "method not found" } });
});
"#;

/// A wave-2 kernel (not started) that answers every consent card with its first option (allow / give).
pub fn wave2_kernel(ws: &Path) -> anyhow::Result<(Arc<Kernel>, Arc<Collect>)> {
    std::fs::create_dir_all(ws.join("inbox"))?;
    println!("workspace {}  state {}  vm home {}", ws.display(), openapp_lib::state::state_dir(ws).display(), openapp_lib::vm::vm_home().display());
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.to_path_buf(), None, None, sink.clone())?;
    auto_answer(&k, &sink, first_option);
    Ok((k, sink))
}

/// Delete the wave-2 Keychain items.
pub fn wave2_cleanup(k: &Kernel) {
    for n in ["wave2d-key", "wave2d-pw", "wave2d-bridge"] {
        let _ = k.secrets.delete(n);
    }
}
