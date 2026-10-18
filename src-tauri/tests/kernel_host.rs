//! Host suite (plain `cargo test` on macOS; no VM, no provider): the kernel over MCP + sessions, providers, skills,
//! per-turn undo, host audio (kernel_smoke); §20 connect: secrets, net.fetch, OAuth, IMAP, downloads, guide
//! (connect_smoke); §18 macOS host intelligence + host modules (macos_smoke); the host halves of §21 G1/G9 live
//! channels + lifecycle (channels_host) and of §21 wave 2: jobs, background, secret.generate, host services,
//! on-device agents (wave2_host). The VM halves are in `vm_suite.rs`.
//!
//! Each case runs in its own process with a scratch workspace, state home, host-module build dir and Keychain
//! prefix (`support::isolated`); Keychain items a case leaves behind are deleted by the harness.
//! Needs node + npm (the MCP SDK is installed once into the test target), swiftc, a logged-in macOS session and
//! Apple Intelligence (wave2_host skips its on-device agent run without it, or with OPENAPP_SMOKE_NO_AI=1).

#[macro_use]
mod support;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use openapp_lib::kernel::Kernel;
use openapp_lib::primitives::channels::{self, ChannelData};
use openapp_lib::primitives::{self, Caller, agents, background, jobs, lifecycle};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use support::fixtures::{self, CHANNELS_ENV, WAVE2_ENV, WAVE2_SERVER_TS};
use support::{Collect, Ctx, Opts, auto_answer, isolated, ms, status, text_of, wait_for, write, write_module};

// ================================================================ kernel_smoke: the kernel over MCP, headless

#[test]
fn kernel_smoke() {
    isolated("kernel_smoke", Opts::default(), kernel_smoke_case);
}

const KERNEL_SERVER_TS: &str = r#"import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { writeFileSync } from "node:fs";

let count = 0;
const server = new McpServer({ name: "hello", version: "0.1.0" });
server.registerTool("greet", { description: "Say hello", inputSchema: { name: z.string().optional() } },
  async ({ name }) => ({
    content: [{ type: "text", text: `Hello ${name ?? "there"}` }],
    structuredContent: { openapp: { notify: `Hello ${name ?? "there"}` } },
  }));
server.registerTool("transcribe", { description: "Fake transcription", inputSchema: { path: z.string(), mime: z.string().optional() } }, async ({ path, mime }) => {
  const { statSync } = await import("node:fs");
  const text = `heard ${statSync(path).size} bytes (${mime})`;
  return { content: [{ type: "text", text }], structuredContent: { openapp: { composer_insert: text } } };
});
server.registerTool("bump", { description: "Increment a counter and rewrite the button view" }, async () => {
  count++;
  writeFileSync("views/button.json", JSON.stringify({ type: "stack", direction: "row", children: [
    { type: "icon-button", icon: "mic", label: `Count ${count}`, action: { tool: "bump" }, active: count % 2 === 1 },
    { type: "slot", id: "hello-extra" } ] }));
  console.error(`bumped to ${count}`);
  return { content: [{ type: "text", text: String(count) }], structuredContent: { count, openapp: { composer_insert: `count=${count}` } } };
});
await server.connect(new StdioServerTransport());
"#;

const FAKE_ACP_TS: &str = r#"import * as readline from "node:readline";
const rl = readline.createInterface({ input: process.stdin });
let n = 0;
let reqId = 1000;
const waiting = new Map<number, (r: any) => void>();
const send = (m: unknown) => process.stdout.write(JSON.stringify(m) + "\n");
const say = (sessionId: string, text: string) => send({ jsonrpc: "2.0", method: "session/update", params: { sessionId, update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text } } } });
rl.on("line", async (line: string) => {
  const m = JSON.parse(line);
  if (m.method === undefined && waiting.has(m.id)) { waiting.get(m.id)!(m.result ?? m.error); waiting.delete(m.id); return; }
  if (m.method === "initialize") send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: 1, agentCapabilities: { loadSession: false, mcpCapabilities: { http: true } }, agentInfo: { name: "fake-acp", title: "Fake ACP" } } });
  else if (m.method === "session/new") send({ jsonrpc: "2.0", id: m.id, result: { sessionId: `fs_${++n}` } });
  else if (m.method === "session/prompt") {
    const blocks: string[] = m.params.prompt.map((p: { text?: string }) => p.text ?? "");
    const last = blocks[blocks.length - 1];
    const sid = m.params.sessionId;
    if (last === "ask") {
      const id = ++reqId;
      const r: any = await new Promise((res) => { waiting.set(id, res); send({ jsonrpc: "2.0", id, method: "elicitation/create", params: { sessionId: sid, message: "Which color?", requestedSchema: { type: "object", properties: { color: { type: "string", enum: ["red", "blue"], description: "Pick a color" } } } } }); });
      say(sid, `answer: ${r?.action}:${r?.content?.color ?? ""}`);
    } else {
      const tags = [blocks.some((b) => b.startsWith("You are the builder inside OpenApp")) ? "contain" : "", blocks.some((b) => b.startsWith("OpenApp build mode.")) ? "build" : ""].filter(Boolean).join(",");
      say(sid, `echo: ${last} [${tags}]`);
    }
    send({ jsonrpc: "2.0", id: m.id, result: { stopReason: "end_turn" } });
  } else if (m.id !== undefined) send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "not supported" } });
});
"#;

async fn kernel_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let ws = cx.ws;
    support::mcp_sdk(&ws)?;
    let m = ws.join("modules/hello");
    write(&m.join("server.ts"), KERNEL_SERVER_TS);
    write(
        &m.join("openapp.json"),
        r#"{ "id": "hello", "name": "Hello", "server": { "command": ["node", "server.ts"] },
            "views": { "button": { "kind": "spec", "file": "views/button.json" },
                       "panel": { "kind": "html", "file": "views/panel.html" } },
            "placements": [ { "view": "button", "anchor": "composer.actions" } ] }"#,
    );
    write(
        &m.join("views/button.json"),
        r#"{ "type": "stack", "direction": "row", "children": [
              { "type": "icon-button", "icon": "mic", "label": "Count 0", "action": { "tool": "bump" } },
              { "type": "slot", "id": "hello-extra" } ] }"#,
    );
    write(&m.join("views/panel.html"), "<!doctype html><html><head><title>p</title></head><body><p>panel</p></body></html>");
    // a broken module to exercise structured errors
    write(&ws.join("modules/broken/openapp.json"), r#"{ "id": "nope", "views": { "x": { "kind": "weird", "file": "../x" } } }"#);

    write(&ws.join("fake-acp.ts"), FAKE_ACP_TS);
    let fake_cmd = format!("node {}", ws.join("fake-acp.ts").display());
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.clone(), Some(fake_cmd.clone()), None, sink.clone())?;
    check!(ws.join("AGENTS.md").exists() && ws.join("CLAUDE.md").exists(), "first run wrote AGENTS.md + CLAUDE.md");
    check!(k.state_dir.is_dir() && !k.state_dir.starts_with(&ws), "kernel state lives outside the workspace in {}", k.state_dir.display());
    k.start(false).await?;
    let ep = k.mcp.get().unwrap();
    println!("MCP server: {}", ep.url);

    // wrong token is rejected
    let bad = StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(ep.url.clone()).auth_header("nope"));
    check!(().serve(bad).await.is_err(), "wrong bearer token rejected");

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(ep.url.clone()).auth_header(ep.token.clone()),
    );
    let client = ().serve(transport).await?;
    let info = client.peer_info().unwrap();
    check!(
        info.instructions.as_deref().is_some_and(|i| i.contains("Call openapp_describe first")),
        "server instructions present (protocol {})",
        info.protocol_version
    );
    let tools = client.list_all_tools().await?;
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    check!(
        names.iter().filter(|n| n.starts_with("openapp_")).count() == 21 + openapp_lib::primitives::mcp_tools().iter().filter(|t| t.name.starts_with("openapp_")).count()
            && names.iter().filter(|n| n.starts_with("host_")).count() == 10,
        "8 + 13 (§20 connect) openapp_* + §21 primitive tools + 10 host_* tools listed: {names:?}"
    );

    let call = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let mut p = CallToolRequestParams::new(name);
            if let Value::Object(o) = args {
                p = p.with_arguments(o);
            }
            client.call_tool(p).await.expect("call_tool transport")
        }
    };

    let r = call("openapp_describe", json!({})).await;
    let d = r.structured_content.clone().unwrap();
    check!(d["workspace"].as_str().is_some() && d["contract"] == "0.10", "openapp_describe: workspace {}", d["workspace"]);
    println!("     host: {}", d["host"]);
    println!("     modules after autoload: {}", json!(d["modules"].as_array().unwrap().iter().map(|m| json!([m["id"], m["status"]])).collect::<Vec<_>>()));

    let r = call("openapp_module_load", json!({ "id": "broken" })).await;
    check!(r.is_error == Some(true), "broken module rejected: {}", text_of(&r));

    let r = call("openapp_module_load", json!({ "id": "hello" })).await;
    let v = r.structured_content.clone().unwrap();
    check!(r.is_error != Some(true) && v["status"] == "running", "openapp_module_load hello: tools {}", v["tools"].as_array().map(|a| a.len()).unwrap_or(0));
    let snap = k.snapshot();
    check!(
        snap.layout.placements.iter().any(|p| p.module == "hello" && p.anchor == "composer.actions"),
        "default placement applied (first load, during autoload or now)"
    );
    check!(snap.views.get("hello/panel").is_some_and(|v| v["url"] == "oamod://hello/views/panel.html"), "html view url {}", snap.views["hello/panel"]);

    let r = call(
        "openapp_layout",
        json!({ "ops": [
            { "op": "region_add", "id": "left", "edge": "left", "size": 280, "mode": "tabs", "title": "Tools" },
            { "op": "place", "module": "hello", "view": "panel", "anchor": "region:left", "label": "Panel" }
        ] }),
    )
    .await;
    let v = r.structured_content.clone().unwrap();
    check!(r.is_error != Some(true), "openapp_layout region_add+place: placed {}", v["placed"]);
    let panel_pid = v["placed"][0]["placement"].as_str().unwrap().to_string();
    let r = call("openapp_layout", json!({ "ops": [ { "op": "place", "module": "hello", "view": "nope", "anchor": "main" }, { "op": "region_add", "id": "x", "edge": "middle", "size": 10 } ] })).await;
    check!(r.is_error == Some(true), "bad layout ops rejected atomically: {}", r.structured_content.as_ref().unwrap()["errors"]);

    let r = call("openapp_call", json!({ "module": "hello", "tool": "greet", "args": { "name": "Ada" } })).await;
    check!(text_of(&r).contains("Hello Ada"), "openapp_call greet -> notify directive {}", r.structured_content.as_ref().unwrap()["directives_applied"]);
    check!(sink.last("oa://notify").is_some_and(|n| n["text"] == "Hello Ada"), "oa://notify emitted");

    // ui_action on the button placement: tool bump rewrites the view
    let btn_pid = k.snapshot().layout.placements.iter().find(|p| p.view == "button").unwrap().id.clone();
    let res = k.ui_action(&btn_pid, json!({ "tool": "bump" }), None).await;
    check!(res.is_ok(), "ui_action tool bump: {res:?}");
    let tree = k.snapshot().views["hello/button"]["tree"].clone();
    check!(tree["children"][0]["label"] == "Count 1", "view re-read after tool call: {}", tree["children"][0]["label"]);
    check!(sink.last("oa://composer").is_some_and(|c| c["insert"] == "count=1"), "composer_insert directive");
    let res = k.ui_action("chat", json!({ "host": "notify", "text": "from chat" }), None).await;
    check!(res.is_ok() && sink.last("oa://notify").unwrap()["text"] == "from chat", "host action with placement \"chat\"");
    let res = k.ui_action("p_missing", json!({ "tool": "bump" }), None).await;
    check!(res.is_err(), "tool action on unknown placement fails: {}", res.unwrap_err());

    // iframe_call
    let r = k.iframe_call("hello", "greet", json!({ "name": "Iframe" })).await;
    check!(r.as_ref().is_ok_and(|v| v["content"][0]["text"] == "Hello Iframe"), "iframe_call returns CallToolResult JSON");
    check!(k.iframe_call("hello", "missing", json!({})).await.is_err(), "iframe_call unknown tool -> Err");

    // slot placement
    let r = call("openapp_layout", json!({ "ops": [ { "op": "place", "module": "hello", "view": "panel", "anchor": "slot:hello-extra" } ] })).await;
    check!(r.is_error != Some(true), "place into slot:hello-extra");

    // live view edit picked up by the 500 ms watcher
    let before = sink.count("oa://snapshot");
    write(&m.join("views/panel.html"), "<!doctype html><html><head></head><body><p>panel v2</p></body></html>");
    let rev0 = k.snapshot().views["hello/panel"]["rev"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let rev1 = k.snapshot().views["hello/panel"]["rev"].as_u64().unwrap();
    check!(rev1 > rev0 && sink.count("oa://snapshot") > before, "watcher bumped html rev {rev0} -> {rev1} and pushed oa://snapshot");
    write(&m.join("views/button.json"), r#"{ "type": "stack", "children": [ { "type": "sparkle" }, { "type": "slot", "id": "hello-extra" } ] }"#);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    k.report_ui_error(&panel_pid, "TypeError: x is undefined");

    let r = call("openapp_snapshot", json!({})).await;
    let t = text_of(&r);
    println!("----- openapp_snapshot -----\n{t}----------------------------");
    check!(t.contains("unknown type \"sparkle\"") && t.contains("TypeError") && t.contains("region:left"), "snapshot shows tree, view problems and UI errors");

    // protocol handler
    let pr = openapp_lib::protocol::handle(&ws, "oamod://hello/views/panel.html");
    check!(pr.status == 200 && String::from_utf8_lossy(&pr.body).contains("_openapp/bridge.js"), "oamod:// serves html with bridge");
    check!(openapp_lib::protocol::handle(&ws, "oamod://hello/../../AGENTS.md").status == 403, "oamod:// traversal blocked");

    // theme
    let r = call("openapp_theme", json!({ "tokens": { "accent": "#0b7a5c", "radius": 14 } })).await;
    check!(r.is_error != Some(true), "openapp_theme valid accepted");
    let r = call("openapp_theme", json!({ "tokens": { "text": "#bbbbbb" } })).await;
    check!(r.is_error == Some(true), "openapp_theme low contrast rejected: {}", r.structured_content.as_ref().unwrap()["errors"][0]);
    check!(k.snapshot().theme["accent"] == "#0b7a5c", "theme kept the valid change");

    // undo: theme change, then the slot placement
    let r = call("openapp_undo", json!({ "steps": 2 })).await;
    let snap = k.snapshot();
    check!(
        r.is_error != Some(true) && snap.theme["accent"] == "#3e63dd" && !snap.layout.placements.iter().any(|p| p.anchor == "slot:hello-extra"),
        "openapp_undo 2 steps reverted theme + slot placement"
    );
    k.send_prompt("/undo".into());
    check!(k.snapshot().layout.regions.is_empty(), "/undo reverted region_add");
    k.send_prompt("/reset".into());
    let snap = k.snapshot();
    check!(snap.layout.placements.is_empty() && snap.modules.iter().any(|m| m.id == "hello"), "/reset cleared layout, kept modules");

    let r = call("openapp_module_unload", json!({ "id": "hello" })).await;
    check!(r.is_error != Some(true) && k.snapshot().modules.iter().any(|m| m.id == "hello" && m.status == "stopped"), "openapp_module_unload");

    // ---------- sessions (§10) ----------
    let snap = k.snapshot();
    check!(snap.sessions.len() == 1 && snap.active_session == snap.sessions[0].id, "first run created one session {}", snap.active_session);
    let s1 = snap.active_session.clone();
    check!(k.ui_action("chat", json!({ "host": "session.rename", "id": s1, "title": "First chat" }), None).await.is_ok(), "session.rename via ui_action(\"chat\")");
    k.ui_action("chat", json!({ "host": "session.new" }), None).await.map_err(anyhow::Error::msg)?;
    let snap = k.snapshot();
    let s2 = snap.active_session.clone();
    check!(s2 != s1 && snap.chat.is_empty() && snap.sessions.len() == 2, "session.new -> {s2} active, empty chat");
    k.ui_action("chat", json!({ "host": "session.new" }), None).await.map_err(anyhow::Error::msg)?;
    check!(k.snapshot().sessions.len() == 2 && k.active_session() == s2, "session.new on an empty untitled session reuses it");
    k.send_prompt("Give me a left siderail with sessions, please, and make it pretty".into());
    tokio::time::sleep(Duration::from_millis(400)).await;
    let snap = k.snapshot();
    let t2 = snap.sessions.iter().find(|x| x.id == s2).unwrap().title.clone();
    check!(t2.chars().count() <= 48 && t2.starts_with("Give me a left siderail"), "title from first prompt: {t2:?}");
    check!(snap.chat.len() == 2 && snap.chat[1]["kind"] == "error", "turn routed to {s2} (no harness: error item {:?})", snap.chat[1]["text"]);
    check!(snap.sessions[0].id == s2, "sessions sorted newest updated first");
    check!(k.ui_action("p_any", json!({ "host": "session.open", "id": s1 }), None).await.is_ok() && k.active_session() == s1, "session.open (iframe-style host action)");
    check!(k.snapshot().chat.iter().any(|i| i["text"].as_str().is_some_and(|t| t.starts_with("Reset"))), "active chat switched to {s1}");
    check!(k.ui_action("chat", json!({ "host": "session.open", "id": "s_nope" }), None).await.is_err(), "session.open unknown id fails");
    // host data binding in a spec view
    write(&ws.join("modules/rail/openapp.json"), r#"{ "id": "rail", "name": "Sessions rail", "views": { "rail": { "kind": "spec", "file": "views/rail.json" } } }"#);
    write(&ws.join("modules/rail/views/rail.json"), r#"{ "type": "stack", "children": [
        { "type": "button", "label": "New", "icon": "plus", "action": { "host": "session.new" } },
        { "type": "text", "value": { "$host": "session.active.title" }, "variant": "title" },
        { "type": "list", "items": { "$host": "sessions" }, "empty": "No sessions" },
        { "type": "text", "value": { "$host": "nope" } } ] }"#);
    let r = call("openapp_module_load", json!({ "id": "rail" })).await;
    let warn = r.structured_content.as_ref().unwrap()["warnings"].to_string();
    check!(r.is_error != Some(true) && warn.contains("unknown host data") && !warn.contains("items must"), "$host bindings validated: {warn}");
    call("openapp_layout", json!({ "ops": [ { "op": "region_add", "id": "rail", "edge": "left", "size": 240 }, { "op": "place", "module": "rail", "view": "rail", "anchor": "region:rail" } ] })).await;
    let t = text_of(&call("openapp_snapshot", json!({})).await);
    check!(t.contains("list (host data: sessions)") && t.contains("2 session(s)"), "snapshot text shows host data binding");
    // web view kind (§13): manifest + snapshot; surfaces need the Tauri host
    write(&ws.join("modules/browser/openapp.json"), r#"{ "id": "browser", "name": "Browser", "views": { "gemini": { "kind": "web", "url": "https://gemini.google.com/app", "profile": "google" } } }"#);
    let r = call("openapp_module_load", json!({ "id": "browser" })).await;
    let v = k.snapshot().views.get("browser/gemini").cloned().unwrap_or(json!(null));
    check!(r.is_error != Some(true) && v == json!({ "kind": "web", "url": "https://gemini.google.com/app", "profile": "google" }), "web view in snapshot: {v}");
    let r = call("openapp_layout", json!({ "ops": [ { "op": "place", "module": "browser", "view": "gemini", "anchor": "main" } ] })).await;
    let wpid = r.structured_content.as_ref().unwrap()["placed"][0]["placement"].as_str().unwrap_or("").to_string();
    let wb = k.web_bounds(&wpid, Some(openapp_lib::kernel::WebRect { x: 0.0, y: 0.0, width: 400.0, height: 300.0 }));
    check!(wb.is_err(), "web_bounds headless reports: {:?}", wb.err());
    call("openapp_layout", json!({ "ops": [ { "op": "unplace", "placement": wpid } ] })).await;
    let d = call("openapp_describe", json!({})).await.structured_content.unwrap();
    check!(
        d["sessions"].as_array().is_some_and(|a| a.len() == 2) && d["activeSession"] == s1.as_str() && d["host_actions"]["session.open"].is_string() && d["host_data"]["sources"]["sessions"].is_string(),
        "openapp_describe lists sessions, activeSession, host data + actions"
    );

    // ---------- provider picker (§11) ----------
    let snap = k.snapshot();
    let ids: Vec<&str> = snap.harnesses.iter().map(|h| h.id.as_str()).collect();
    check!(
        ids == ["codex", "pi", "claude", "gemini", "goose", "opencode", "custom"] && snap.agent.status == "unselected" && snap.agent.harness.is_none(),
        "detection: {}",
        snap.harnesses.iter().map(|h| format!("{}={}/{:?} ({})", h.id, h.available, h.signed_in, h.note.clone().unwrap_or_default())).collect::<Vec<_>>().join("; ")
    );
    check!(k.ui_action("chat", json!({ "host": "harness.select", "id": "nope" }), None).await.is_err(), "harness.select unknown id fails");
    check!(k.ui_action("chat", json!({ "host": "harness.select", "id": "custom" }), None).await.is_ok(), "harness.select custom (fake ACP agent)");
    let t0 = std::time::Instant::now();
    while k.agent_status().0 != "ready" && t0.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let snap = k.snapshot();
    check!(snap.agent.status == "ready" && snap.agent.harness.as_deref() == Some("custom"), "custom harness ready in {:?} as {:?}", t0.elapsed(), snap.agent.name);
    check!(std::fs::read_to_string(k.state_dir.join("harness.json")).is_ok_and(|t| t.contains("\"custom\"")), "choice persisted in <state>/harness.json");
    k.send_prompt("ping".into());
    let t0 = std::time::Instant::now();
    while !k.snapshot().chat.iter().any(|i| i["kind"] == "agent") && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let agent = k.snapshot().chat.iter().rev().find(|i| i["kind"] == "agent").cloned().unwrap_or(json!(null));
    check!(agent["text"] == "echo: ping [contain]", "ACP turn routed to the active session, first prompt carries containment: {}", agent["text"]);
    let idx = std::fs::read_to_string(k.state_dir.join("sessions/index.json"))?;
    check!(idx.contains("\"custom\": \"fs_1\""), "agent session id stored per harness in sessions/index.json");
    k.send_prompt("/provider".into());
    check!(k.snapshot().agent.status == "unselected" && !k.state_dir.join("harness.json").exists(), "/provider returns to the picker");
    k.ui_action("chat", json!({ "host": "harness.select", "id": "custom" }), None).await.map_err(anyhow::Error::msg)?;
    let d = call("openapp_describe", json!({})).await.structured_content.unwrap();
    check!(d["harnesses"].as_array().is_some_and(|a| a.len() == 7) && d["host_data"]["sources"]["harnesses"].is_string() && d["host_data"]["sources"]["grants"].is_string() && d["capabilities"]["vm"] == false, "describe lists harnesses + host data sources (grants), capabilities.vm false in host mode");
    check!(
        d["capabilities"]["audio"].is_string() && d["kernel_gaps"].as_array().is_some_and(|a| a.len() >= 3) && d["capabilities"]["views"]["web"].is_string(),
        "describe lists capabilities + kernel_gaps"
    );
    while k.agent_status().0 != "ready" {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ---------- skills + modes (§12) ----------
    check!(
        std::fs::read_to_string(ws.join(".openapp/skills/openapp-builder/SKILL.md")).is_ok_and(|t| t.starts_with("---\nname: openapp-builder"))
            && ws.join(".claude/skills/openapp-builder/SKILL.md").exists()
            && ws.join(".openapp/skills/openapp-builder/reference/host-modules.md").exists()
            && std::fs::read_to_string(ws.join("AGENTS.md")).is_ok_and(|t| t.contains("Build mode") && t.lines().count() < 30),
        "builder skill + reference files written (+ .claude mirror), AGENTS.md is short"
    );
    check!(k.snapshot().active_mode == "chat", "sessions start in chat mode");
    let wait_agent = |needle: &'static str| {
        let k = k.clone();
        async move {
            support::poll(Duration::from_secs(10), || k.snapshot().chat.iter().rev().find(|i| i["kind"] == "agent" && i["text"].as_str().is_some_and(|t| t.contains(needle))).cloned()).await.unwrap_or(json!(null))
        }
    };
    k.send_prompt("/build make it blue".into());
    let a = wait_agent("make it blue").await;
    let snap = k.snapshot();
    check!(
        snap.active_mode == "build" && snap.sessions.iter().any(|x| x.id == snap.active_session && x.mode == "build") && a["text"].as_str().is_some_and(|t| t.contains("build]")),
        "/build <text> switches to build mode and the turn carries the skill prefix: {}",
        a["text"]
    );
    check!(k.ui_action("chat", json!({ "host": "mode.set", "mode": "chat" }), None).await.is_ok() && k.snapshot().active_mode == "chat", "mode.set chat");
    // question round trip (fake ACP elicitation -> question item -> answer_question)
    k.send_prompt("ask".into());
    let q = support::open_item(&k, "question", Duration::from_secs(10)).await.unwrap_or(json!(null));
    check!(q["questions"][0]["id"] == "color" && q["questions"][0]["options"][1]["label"] == "blue", "question item rendered: {}", q["questions"]);
    let mut ans = serde_json::Map::new();
    ans.insert("color".into(), json!(["blue"]));
    support::answer(|| k.answer_question(q["id"].as_str().unwrap_or(""), Some(ans.clone()))).await.map_err(anyhow::Error::msg)?;
    let a = wait_agent("answer:").await;
    check!(a["text"] == "answer: accept:blue", "answer_question reached the agent: {}", a["text"]);

    // ---------- §19.2 per-turn undo ----------
    // a build turn that waits on a question; MCP changes made meanwhile belong to that turn
    let theme_before = k.snapshot().theme;
    let layout_before = k.snapshot().layout.placements.len();
    k.send_prompt("/build ask".into());
    let q2 = support::open_item(&k, "question", Duration::from_secs(10)).await.unwrap_or(json!(null));
    write(&ws.join("modules/extra/openapp.json"), r#"{ "id": "extra", "name": "Extra", "views": { "v": { "kind": "spec", "file": "v.json" } } }"#);
    write(&ws.join("modules/extra/v.json"), r#"{ "type": "text", "value": "extra" }"#);
    let r1 = call("openapp_module_load", json!({ "id": "extra" })).await;
    let r2 = call("openapp_layout", json!({ "ops": [ { "op": "place", "module": "extra", "view": "v", "anchor": "composer.above" } ] })).await;
    let r3 = call("openapp_theme", json!({ "tokens": { "accent": "#7c3aed" } })).await;
    check!(r1.is_error != Some(true) && r2.is_error != Some(true) && r3.is_error != Some(true), "build turn: module loaded, view placed, theme changed");
    let mut ans = serde_json::Map::new();
    ans.insert("color".into(), json!(["red"]));
    support::answer(|| k.answer_question(q2["id"].as_str().unwrap_or(""), Some(ans.clone()))).await.map_err(anyhow::Error::msg)?;
    let a = wait_agent("answer: accept:red").await;
    let snap = k.snapshot();
    let tid = a["turnId"].as_str().unwrap_or("").to_string();
    let user = snap.chat.iter().rev().find(|i| i["kind"] == "user").cloned().unwrap_or_default();
    check!(!tid.is_empty() && a["mode"] == "build" && user["turnId"] == tid.as_str() && user["mode"] == "build", "build turn items share turnId {tid} and mode build");
    let rec = snap.turns.iter().find(|t| t["id"] == tid.as_str()).cloned().unwrap_or_default();
    check!(rec["undoable"] == true && rec["changes"] == 2 && rec["modules"] == json!(["extra"]), "snapshot.turns records the turn: {rec}");
    let r = k.ui_action("chat", json!({ "host": "turn.undo", "turnId": tid }), None).await;
    let snap = k.snapshot();
    check!(
        r.is_ok() && snap.theme == theme_before && snap.layout.placements.len() == layout_before
            && snap.modules.iter().any(|m| m.id == "extra" && m.status == "stopped") && ws.join("modules/extra/openapp.json").exists(),
        "turn.undo reverted layout + theme, unloaded the module, kept its files"
    );
    check!(k.ui_action("chat", json!({ "host": "turn.undo", "turnId": tid }), None).await.is_err(), "a turn cannot be undone twice");
    k.ui_action("chat", json!({ "host": "mode.set", "mode": "chat" }), None).await.map_err(anyhow::Error::msg)?;

    // ---------- host audio (§15.1) ----------
    let r = call("openapp_module_load", json!({ "id": "hello" })).await;
    check!(r.is_error != Some(true), "hello reloaded for the audio test");
    let r = call("openapp_layout", json!({ "ops": [ { "op": "place", "module": "hello", "view": "button", "anchor": "composer.actions" } ] })).await;
    let pid = r.structured_content.as_ref().unwrap()["placed"][0]["placement"].as_str().unwrap_or("").to_string();
    let audio_action = json!({ "host": "audio.record", "tool": "transcribe", "arg": "path" });
    let early = openapp_lib::primitives::audio::save(&k, &pid, json!("UklGRg=="), "audio/webm", Some(audio_action.clone()), None).await;
    check!(early.is_err(), "save_audio before consent is refused: {:?}", early.err());
    let k2c = k.clone();
    let (pid2, act2) = (pid.clone(), audio_action.clone());
    let consent = tokio::spawn(async move { k2c.ui_action(&pid2, act2, None).await });
    let card = support::poll(Duration::from_secs(5), || {
        k.snapshot().chat.iter().rev().find(|i| i["kind"] == "permission" && i["title"].as_str().is_some_and(|t| t.contains("microphone")) && i.get("resolved").is_none()).cloned()
    })
    .await
    .unwrap_or(json!(null));
    check!(card["options"][0]["optionId"] == "allow", "microphone consent card: {}", card["title"]);
    support::answer(|| k.respond_permission(card["id"].as_str().unwrap_or(""), Some("allow".into()))).await.map_err(anyhow::Error::msg)?;
    check!(consent.await.is_ok_and(|r| r.is_ok()), "audio.record ui_action ok after Allow");
    let grants: Value = std::fs::read(k.state_dir.join("grants.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    check!(grants["microphone"]["hello"]["allowed"] == true && grants["microphone"]["hello"]["scope"] == "workspace", "grant stored in <state>/grants.json: {}", grants["microphone"]);
    let before = sink.count("oa://composer");
    let res = openapp_lib::primitives::audio::save(&k, &pid, json!("UklGRiQAAABXQVZF"), "audio/webm", Some(audio_action.clone()), Some(1200)).await;
    let ins = sink.last("oa://composer").unwrap_or(json!(null));
    check!(
        res.is_ok() && sink.count("oa://composer") > before && ins["insert"] == "heard 12 bytes (audio/webm)",
        "save_audio -> file -> tool {{path, mime}} -> composer_insert: {}",
        ins["insert"]
    );
    let recs = std::fs::read_dir(ws.join("modules/hello/data/recordings")).map(|d| d.count()).unwrap_or(0);
    check!(recs >= 1, "recording written to modules/hello/data/recordings ({recs})");

    let research = std::fs::read_to_string(k.state_dir.join("research.jsonl"))?;
    let kinds: std::collections::BTreeSet<String> = research
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["kind"].as_str().map(String::from))
        .collect();
    check!(!research.contains(&ep.token), "research log has no token; kinds: {kinds:?}");
    let log = std::fs::read_to_string(ws.join(".openapp/logs/hello.log")).unwrap_or_default();
    check!(log.contains("bumped to 1"), "module stderr captured in .openapp/logs/hello.log");

    check!(
        ["session_new", "session_open", "harness_select", "question", "mode", "audio_record"].iter().all(|x| kinds.contains(*x)),
        "research kinds session_new, session_open, harness_select, question, mode, audio_record"
    );
    client.cancel().await?;
    k.shutdown();
    // ---------- persistence across a kernel restart ----------
    let k2 = Kernel::new_opts(ws.clone(), Some(fake_cmd.clone()), None, Arc::new(Collect::default()))?;
    check!(k2.snapshot().agent.harness.as_deref() == Some("custom"), "restart: saved provider selected without a picker");
    let snap = k2.snapshot();
    check!(
        snap.sessions.len() == 2 && snap.active_session == s1 && snap.sessions.iter().any(|x| x.id == s1 && x.title == "First chat"),
        "restart: 2 sessions, active {s1} and its title persisted"
    );
    k2.session_open(&s2).map_err(anyhow::Error::msg)?;
    let c2 = k2.snapshot().chat;
    check!(c2.len() == 2 && c2[0]["kind"] == "user", "restart: {s2} chat loaded from <state>/sessions/{s2}.json");
    check!(k.state_dir.join("sessions/index.json").exists(), "sessions/index.json written");
    k2.shutdown();
    println!("ALL PASS ({} snapshot events, {} chat events)", sink.count("oa://snapshot"), sink.count("oa://chat"));
    Ok(())
}

// ================================================================ connect_smoke: §20 connect (no agent)

#[test]
fn connect_smoke() {
    isolated("connect_smoke", Opts { env: &[("OPENAPP_TEST_ALLOW_LOOPBACK", "1")], keychain: true, ..Opts::default() }, connect_smoke_case);
}

const TOKEN: &str = "tok_SMOKE_9f8e7d6c5b4a";
const IMAP_PASS: &str = "imap-pass-SMOKE-31337";
const CLIENT_SECRET: &str = "GOCSPX-smoke-client-secret-42";
const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Default)]
struct Srv {
    last_auth: Option<String>,
    last_key: Option<String>,
    last_query: Option<String>,
    challenge: Option<String>,
    token_calls: Vec<String>,
    api_auth: Vec<String>,
}
type S = Arc<Mutex<Srv>>;

async fn echo(State(s): State<S>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from);
    let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(String::from);
    let mut g = s.lock().unwrap();
    g.last_auth = auth.clone();
    g.last_key = key.clone();
    g.last_query = q.get("key").cloned();
    // echo everything back: the kernel must scrub the credential
    ([("x-echo-auth", auth.clone().unwrap_or_default())], axum::Json(json!({ "auth": auth, "key": key, "query": q.get("key") }))).into_response()
}

async fn authorize(State(s): State<S>, Query(q): Query<HashMap<String, String>>) -> Response {
    let ok = q.get("response_type").map(String::as_str) == Some("code") && q.get("code_challenge_method").map(String::as_str) == Some("S256") && q.get("client_id").map(String::as_str) == Some("smoke-client.apps.example");
    if !ok {
        return (StatusCode::BAD_REQUEST, "bad authorize request").into_response();
    }
    s.lock().unwrap().challenge = q.get("code_challenge").cloned();
    let loc = format!("{}?code=CODE-1&state={}", q["redirect_uri"], q["state"]);
    (StatusCode::FOUND, [("location", loc)]).into_response()
}

async fn token(State(s): State<S>, body: String) -> Response {
    let f: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let mut g = s.lock().unwrap();
    let grant = f.get("grant_type").cloned().unwrap_or_default();
    g.token_calls.push(grant.clone());
    if f.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "invalid_client" }))).into_response();
    }
    match grant.as_str() {
        "authorization_code" => {
            let v = f.get("code_verifier").cloned().unwrap_or_default();
            let ch = B64URL.encode(Sha256::digest(v.as_bytes()));
            if f.get("code").map(String::as_str) != Some("CODE-1") || g.challenge.as_deref() != Some(ch.as_str()) {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "invalid_grant", "error_description": "PKCE mismatch" }))).into_response();
            }
            // expires within the 60 s margin: the first use refreshes
            axum::Json(json!({ "access_token": "AT1-smoke-access", "refresh_token": "RT1-smoke-refresh", "expires_in": 30, "token_type": "Bearer", "scope": "read write" })).into_response()
        }
        "refresh_token" if f.get("refresh_token").map(String::as_str) == Some("RT1-smoke-refresh") => {
            axum::Json(json!({ "access_token": "AT2-smoke-access", "expires_in": 3600, "token_type": "Bearer" })).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "unsupported_grant_type" }))).into_response(),
    }
}

async fn api(State(s): State<S>, headers: HeaderMap) -> Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    s.lock().unwrap().api_auth.push(auth.clone());
    if auth == "Bearer AT2-smoke-access" {
        axum::Json(json!({ "ok": true, "user": "smoke" })).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "expired").into_response()
    }
}

async fn fake_imap(listener: tokio::net::TcpListener) {
    while let Ok((sock, _)) = listener.accept().await {
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut lines = tokio::io::BufReader::new(r).lines();
            let _ = w.write_all(b"* OK fake IMAP ready\r\n").await;
            while let Ok(Some(line)) = lines.next_line().await {
                let (tag, cmd) = line.split_once(' ').unwrap_or((&line, ""));
                let up = cmd.to_ascii_uppercase();
                let out = if up.starts_with("LOGIN") {
                    if cmd.contains(&format!("\"{IMAP_PASS}\"")) { format!("{tag} OK LOGIN done\r\n") } else { format!("{tag} NO bad password\r\n") }
                } else if up.starts_with("LIST") {
                    format!("* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren) \"/\" {{10}}\r\nSent Items\r\n{tag} OK LIST done\r\n")
                } else if up.starts_with("EXAMINE") {
                    format!("* 3 EXISTS\r\n* FLAGS (\\Seen)\r\n{tag} OK [READ-ONLY] EXAMINE done\r\n")
                } else if up.starts_with("UID SEARCH") {
                    format!("* SEARCH 11 12 13\r\n{tag} OK SEARCH done\r\n")
                } else if up.starts_with("UID FETCH") {
                    let mut o = String::new();
                    for (i, uid) in [12, 13].iter().enumerate() {
                        let hdr = format!("Subject: =?UTF-8?B?SGVsbG8gV8O2cmxk?= {uid}\r\nFrom: Ann <ann@example.org>\r\n\r\n");
                        o.push_str(&format!("* {} FETCH (UID {uid} FLAGS (\\Seen) INTERNALDATE \"17-Oct-2026 10:00:00 +0000\" RFC822.SIZE 120 BODY[HEADER.FIELDS (FROM SUBJECT)] {{{}}}\r\n{hdr})\r\n", i + 2, hdr.len()));
                    }
                    o.push_str(&format!("{tag} OK FETCH done\r\n"));
                    o
                } else if up.starts_with("LOGOUT") {
                    format!("* BYE\r\n{tag} OK LOGOUT done\r\n")
                } else {
                    format!("{tag} BAD unknown\r\n")
                };
                let _ = w.write_all(out.as_bytes()).await;
                if up.starts_with("LOGOUT") {
                    break;
                }
            }
        });
    }
}

async fn connect_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    let (ws, state_home) = (cx.ws, cx.state_home);
    let prefix = std::env::var("OPENAPP_KEYCHAIN_PREFIX")?;
    let t_all = Instant::now();
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    check!(k.state_dir.starts_with(&state_home), "scratch state dir {}", k.state_dir.display());
    // auto-answer kernel cards like a user would
    auto_answer(&k, &sink, |c| {
        let title = c["title"].as_str().unwrap_or("");
        Some(if title.contains("localhost") {
            "deny".into()
        } else if title.contains("send") {
            "allow".into()
        } else if title.contains("OAuth client") {
            "save".into()
        } else if title.to_lowercase().contains("delete") {
            "delete".into()
        } else {
            c["options"][0]["optionId"].as_str().unwrap_or("allow").to_string()
        })
    });
    // the "system browser": follow the authorize redirect into the loopback callback
    let browser = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build()?;
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let (b, opened) = (browser.clone(), opened.clone());
        let _ = k.url_opener.set(Arc::new(move |url: &str| {
            opened.lock().unwrap().push(url.to_string());
            let (b, url) = (b.clone(), url.to_string());
            tokio::spawn(async move {
                let Ok(r) = b.get(&url).send().await else { return };
                if let Some(loc) = r.headers().get("location").and_then(|v| v.to_str().ok()) {
                    let page = b.get(loc).send().await.ok();
                    let html = match page {
                        Some(p) => p.text().await.unwrap_or_default(),
                        None => String::new(),
                    };
                    println!("  [browser] callback page: {}", if html.contains("You can close this tab") { "\"You can close this tab\"" } else { "?" });
                }
            });
            Ok(())
        }));
    }
    k.start(false).await?;

    // local test servers
    let srv: S = Arc::new(Mutex::new(Srv::default()));
    let app = axum::Router::new()
        .route("/echo", axum::routing::get(echo))
        .route("/authorize", axum::routing::get(authorize))
        .route("/token", axum::routing::post(token))
        .route("/api", axum::routing::get(api))
        .with_state(srv.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    tokio::spawn(async move { axum::serve(l, app).await });
    let il = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let iport = il.local_addr()?.port();
    tokio::spawn(fake_imap(il));
    let base = format!("http://127.0.0.1:{port}");

    // 1. secret.request → secret card → submit_secret → Keychain
    let req = {
        let k = k.clone();
        tokio::spawn(async move {
            openapp_lib::primitives::connect::call(&k, "secret.request", &json!({ "name": "svc-token", "label": "Smoke service token", "kind": "token", "help": "Create it in the smoke console.", "url": "https://example.org/tokens", "hosts": ["127.0.0.1"] }), "agent").await
        })
    };
    let mut card = Value::Null;
    for _ in 0..200 {
        if let Some(c) = sink.appended("secret").into_iter().last() {
            card = c;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    check!(card["name"] == "svc-token" && card["title"] == "Smoke service token" && card["url"] == "https://example.org/tokens" && card.get("value").is_none(), "secret card shown: {}", card);
    let cid = card["id"].as_str().unwrap_or("").to_string();
    let stored = support::answer(|| k.submit_secret(&cid, Some(format!("  {TOKEN}\n")))).await;
    check!(stored.is_ok(), "submit_secret stored the value");
    let r = req.await?;
    check!(r.as_ref().is_ok_and(|v| v["ok"] == true && v["name"] == "svc-token" && !v.to_string().contains(TOKEN)), "secret.request → {r:?}");
    let item = k.snapshot().chat.into_iter().find(|i| i["id"] == cid.as_str()).unwrap_or_default();
    check!(item["resolved"] == "saved", "secret card resolved \"saved\"");
    let metas = k.secrets_list();
    check!(metas.iter().any(|m| m["name"] == "svc-token" && m["kind"] == "token" && m["hosts"] == json!(["127.0.0.1"])), "host data secrets has metadata: {}", json!(metas));
    let again = openapp_lib::primitives::connect::call(&k, "secret.request", &json!({ "name": "svc-token", "label": "x" }), "agent").await;
    check!(again.as_ref().is_ok_and(|v| v["exists"] == true), "secret.request for an existing name does not ask again");

    // 2. net.fetch with the injected header (bearer, header:, query:) — value scrubbed from replies
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("{base}/echo"), "auth": { "secret": "svc-token" } }), "agent").await;
    let seen = srv.lock().unwrap().last_auth.clone();
    check!(seen.as_deref() == Some(&format!("Bearer {TOKEN}")[..]), "server received Authorization: Bearer <token> (trimmed)");
    let v = r.map_err(anyhow::Error::msg)?;
    check!(v["status"] == 200 && v["json"]["auth"] == "Bearer [secret]" && v["headers"]["x-echo-auth"] == "Bearer [secret]" && !v.to_string().contains(TOKEN), "net.fetch result scrubbed: json.auth={} header={}", v["json"]["auth"], v["headers"]["x-echo-auth"]);
    let _ = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("{base}/echo"), "auth": { "secret": "svc-token", "scheme": "header:X-Api-Key" } }), "agent").await;
    check!(srv.lock().unwrap().last_key.as_deref() == Some(TOKEN), "scheme header:X-Api-Key injected");
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("{base}/echo?a=1"), "auth": { "secret": "svc-token", "scheme": "query:key" } }), "agent").await;
    check!(srv.lock().unwrap().last_query.as_deref() == Some(TOKEN) && r.as_ref().is_ok_and(|v| !v.to_string().contains(TOKEN)), "scheme query:key injected, URL scrubbed");

    // 3. MCP tool + HTTP host endpoint (what module servers use)
    let mcp = openapp_lib::mcp_server::KernelMcp { k: k.clone() };
    let mut a = serde_json::Map::new();
    a.insert("url".into(), json!(format!("{base}/echo")));
    a.insert("auth".into(), json!({ "secret": "svc-token" }));
    let r = mcp.dispatch("openapp_net_fetch", a).await;
    let sc = r.structured_content.clone().unwrap_or_default();
    check!(r.is_error != Some(true) && sc["status"] == 200 && !sc.to_string().contains(TOKEN), "MCP openapp_net_fetch → 200, no value in the result");
    let r = mcp.dispatch("openapp_secrets", serde_json::Map::new()).await;
    check!(r.structured_content.as_ref().is_some_and(|v| v["secrets"].as_array().is_some_and(|a| !a.is_empty()) && !v.to_string().contains(TOKEN)), "MCP openapp_secrets lists names only");
    let ep = k.mcp.get().unwrap();
    let mtok = k.host_token("module:smoke");
    let hr = reqwest::Client::builder().no_proxy().build()?
        .post(format!("http://127.0.0.1:{}/host/net.fetch", ep.port))
        .bearer_auth(&mtok)
        .json(&json!({ "url": format!("{base}/echo"), "auth": { "secret": "svc-token" } }))
        .send()
        .await?;
    let code = hr.status().as_u16();
    let hv: Value = hr.json().await.unwrap_or_default();
    check!(code == 200 && hv["status"] == 200 && hv["json"]["auth"] == "Bearer [secret]", "POST /host/net.fetch (module token) → {code}");
    let used = k.secrets_list().into_iter().find(|m| m["name"] == "svc-token").map(|m| m["used_by"].clone()).unwrap_or_default();
    check!(used.as_array().is_some_and(|a| a.contains(&json!("agent")) && a.contains(&json!("module:smoke"))), "used_by records agent + module: {used}");

    // 4. a secret goes only where the user allowed it
    let cards0 = sink.appended("permission").len();
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("http://localhost:{port}/echo"), "auth": { "secret": "svc-token" } }), "agent").await;
    check!(r.as_ref().is_err_and(|e| e.contains("may only be sent to 127.0.0.1")) && sink.appended("permission").len() == cards0 && srv.lock().unwrap().last_auth.as_deref() != Some("x"), "a secret requested with hosts is held to them (no card): {r:?}");
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": "file:///etc/passwd" }), "agent").await;
    check!(r.is_err(), "net.fetch refuses non-http URLs");

    // 5. download capture: fake OAuth client JSON → card → oauth-client secret (never in the workspace)
    let client_json = json!({ "installed": {
        "client_id": "smoke-client.apps.example", "project_id": "smoke-project",
        "auth_uri": format!("{base}/authorize"), "token_uri": format!("{base}/token"),
        "client_secret": CLIENT_SECRET, "redirect_uris": ["http://localhost"] } });
    let tmp = k.web_download_dest("p_none", "blob:https://console.example/1", "client_secret_smoke.json");
    std::fs::write(&tmp, client_json.to_string())?;
    let r = k.capture_download(None, &tmp, "client_secret_smoke.json").await;
    check!(r.as_ref().is_ok_and(|v| v["secret"] == "oauth-client") && !tmp.exists(), "OAuth client download → card → secret {:?}; temp file removed", r.as_ref().map(|v| v["secret"].clone()));
    let meta = k.secrets.meta("oauth-client").unwrap_or_default();
    check!(meta["kind"] == "oauth-client" && meta["project_id"] == "smoke-project" && !meta.to_string().contains(CLIENT_SECRET), "client metadata without the client secret: {meta}");
    let tmp2 = k.web_download_dest("p_none", "https://example.org/report.csv", "report.csv");
    std::fs::write(&tmp2, "a,b\n1,2\n")?;
    let r = k.capture_download(None, &tmp2, "report.csv").await;
    check!(r.as_ref().is_ok_and(|v| v["path"] == "downloads/report.csv") && ws.join("downloads/report.csv").is_file(), "other download → {:?}", r.map(|v| v["path"].clone()));

    // 6. oauth.authorize: PKCE + loopback callback, token stored; refresh on first use
    let t = Instant::now();
    let r = openapp_lib::primitives::connect::call(&k, "oauth.authorize", &json!({ "client": "oauth-client", "scopes": ["read", "write"], "token": "svc-oauth" }), "agent").await;
    check!(r.as_ref().is_ok_and(|v| v["token"] == "svc-oauth" && v["scopes"] == json!(["read", "write"]) && v["refresh"] == true), "oauth.authorize end to end ({} ms): {r:?}", t.elapsed().as_millis());
    let au = opened.lock().unwrap().first().cloned().unwrap_or_default();
    check!(au.contains("code_challenge=") && au.contains("code_challenge_method=S256") && au.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A") && au.contains("state="), "browser opened the authorize URL with PKCE + loopback redirect");
    let tm = k.secrets.meta("svc-oauth").unwrap_or_default();
    check!(tm["kind"] == "oauth-token" && tm["has_refresh_token"] == true && !tm.to_string().contains("AT1-smoke"), "oauth-token metadata (no token values)");
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("{base}/api"), "auth": { "secret": "svc-oauth" } }), "module:smoke").await;
    let calls = srv.lock().unwrap().token_calls.clone();
    check!(r.as_ref().is_ok_and(|v| v["status"] == 200 && v["json"]["ok"] == true) && calls == vec!["authorization_code", "refresh_token"], "expired token refreshed before use: {calls:?}, api saw {:?}", srv.lock().unwrap().api_auth);
    let tm2 = k.secrets.meta("svc-oauth").unwrap_or_default();
    check!(tm2["expires_at"].as_i64().unwrap_or(0) > tm["expires_at"].as_i64().unwrap_or(0) + 1000, "refresh updated expires_at");
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("{base}/api"), "auth": { "secret": "svc-oauth" } }), "agent").await;
    check!(r.is_ok() && srv.lock().unwrap().token_calls.len() == 2, "fresh token reused (no extra refresh)");
    let r = openapp_lib::primitives::connect::call(&k, "net.fetch", &json!({ "url": format!("http://localhost:{port}/api"), "auth": { "secret": "svc-oauth" } }), "agent").await;
    check!(r.as_ref().is_err_and(|e| e.contains("did not allow")), "a secret without fixed hosts asks per new host; denied → {r:?}");

    // 7. net.imap against a fake server (password via a secret card)
    let req = {
        let k = k.clone();
        tokio::spawn(async move { openapp_lib::primitives::connect::call(&k, "secret.request", &json!({ "name": "mail-pass", "label": "Mail app password", "kind": "password", "hosts": ["127.0.0.1"] }), "agent").await })
    };
    let mut cid = String::new();
    for _ in 0..200 {
        if let Some(c) = sink.appended("secret").into_iter().find(|c| c["name"] == "mail-pass") {
            cid = c["id"].as_str().unwrap_or("").to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    support::answer(|| k.submit_secret(&cid, Some(IMAP_PASS.into()))).await.map_err(anyhow::Error::msg)?;
    req.await?.map_err(anyhow::Error::msg)?;
    let imap = |op: &str, extra: Value| {
        let mut p = json!({ "host": "127.0.0.1", "port": iport, "tls": false, "secret": "mail-pass", "username": "ann@example.org", "op": op });
        for (a, b) in extra.as_object().cloned().unwrap_or_default() {
            p[a] = b;
        }
        let k = k.clone();
        async move { openapp_lib::primitives::connect::call(&k, "net.imap", &p, "agent").await }
    };
    let r = imap("list", json!({})).await;
    check!(r.as_ref().is_ok_and(|v| v["mailboxes"].as_array().is_some_and(|a| a.len() == 2 && a[1]["name"] == "Sent Items")), "net.imap list (literal mailbox name): {r:?}");
    let r = imap("search", json!({ "query": "UNSEEN", "limit": 2 })).await;
    check!(r.as_ref().is_ok_and(|v| v["uids"] == json!([13, 12]) && v["total"] == 3 && v["exists"] == 3), "net.imap search → newest uids: {r:?}");
    let r = imap("fetch", json!({ "uids": [12, 13] })).await;
    check!(r.as_ref().is_ok_and(|v| v["messages"][0]["uid"] == 12 && v["messages"][0]["headers"]["subject"] == "Hello Wörld 12" && v["messages"][1]["headers"]["from"] == "Ann <ann@example.org>"), "net.imap fetch headers (RFC 2047 decoded): {}", r.as_ref().map(|v| v["messages"][0].to_string()).unwrap_or_default());
    let r = openapp_lib::primitives::connect::call(&k, "net.imap", &json!({ "host": "imap.example.org", "tls": false, "secret": "mail-pass", "username": "a", "op": "list" }), "agent").await;
    check!(r.as_ref().is_err_and(|e| e.contains("TLS")), "net.imap refuses plaintext to remote hosts");

    // 8. web control headless + guide strip
    let r = openapp_lib::primitives::connect::call(&k, "web.read", &json!({}), "agent").await;
    check!(r.is_err(), "web.read without a web view → clear error: {:?}", r.err());
    let r = openapp_lib::primitives::connect::call(&k, "guide.set", &json!({ "steps": [{ "title": "Create a project", "detail": "Top bar → project picker → New project" }, { "title": "Enable the Gmail API" }], "current": 0 }), "agent").await;
    check!(r.is_ok() && k.snapshot().guide.as_ref().is_some_and(|g| g["steps"].as_array().is_some_and(|a| a.len() == 2)), "guide.set → snapshot guide");
    k.ui_action_value("present:none", json!({ "host": "guide.done", "step": 0 }), None).await.map_err(anyhow::Error::msg)?;
    let g = k.snapshot().guide.unwrap_or_default();
    let users: Vec<Value> = sink.appended("user");
    check!(g["current"] == 1 && users.iter().any(|u| u["text"] == "Done: Create a project"), "guide Done → current 1 and the agent gets \"Done: Create a project\"");
    k.host_action("guide.clear", &[&json!({})]).map_err(anyhow::Error::msg)?;
    check!(k.snapshot().guide.is_none(), "guide.clear");
    let d = k.describe();
    check!(d["host_actions"]["secret.request"].is_string() && d["host_actions"]["web.read"].is_string() && d["host_data"]["sources"]["secrets"].is_string() && d["capabilities"]["connect"].is_string(), "describe lists the connect actions, host data secrets, capabilities.connect");
    let names: Vec<String> = openapp_lib::mcp_server::tools().iter().map(|t| t.name.to_string()).collect();
    check!(["openapp_secret_request", "openapp_net_fetch", "openapp_net_imap", "openapp_oauth_authorize", "openapp_web_read", "openapp_web_click", "openapp_web_fill", "openapp_web_highlight", "openapp_guide", "openapp_host_action"].iter().all(|n| names.iter().any(|x| x == n)), "MCP tools listed");

    // 9. secret.delete with a confirm card
    let r = openapp_lib::primitives::connect::call(&k, "secret.delete", &json!({ "name": "svc-token" }), "agent").await;
    check!(r.is_ok() && k.secrets.meta("svc-token").is_none() && k.secrets.get("svc-token").is_err(), "secret.delete (confirmed) removed metadata + Keychain item");

    // 10. values never in chat, research, sessions, logs, metadata, events
    k.flush_sessions();
    let mut hay = String::new();
    for dir in [k.state_dir.clone(), ws.join(".openapp")] {
        for e in support::walk(&dir) {
            hay.push_str(&String::from_utf8_lossy(&std::fs::read(&e).unwrap_or_default()));
        }
    }
    hay.push_str(&sink.all_text());
    hay.push_str(&serde_json::to_string(&k.snapshot()).unwrap_or_default());
    let leaks: Vec<&str> = [TOKEN, IMAP_PASS, CLIENT_SECRET, "AT1-smoke-access", "AT2-smoke-access", "RT1-smoke-refresh"].into_iter().filter(|s| hay.contains(s)).collect();
    check!(leaks.is_empty(), "no secret value in state files (research, sessions, secrets.json, grants), workspace logs or kernel events ({} KB checked); leaks: {leaks:?}", hay.len() / 1024);
    let research = std::fs::read_to_string(k.state_dir.join("research.jsonl")).unwrap_or_default();
    check!(research.contains("\"net_fetch\"") && research.contains("\"oauth\"") && research.contains("\"net_imap\"") && research.contains("\"secret\""), "research log has net_fetch / oauth / net_imap / secret entries");

    // cleanup: every Keychain item of this run
    for m in k.secrets_list() {
        let n = m["name"].as_str().unwrap_or("").to_string();
        k.secrets.delete(&n).map_err(anyhow::Error::msg)?;
    }
    let mut left = 0;
    for n in ["svc-token", "svc-oauth", "oauth-client", "mail-pass"] {
        let svc = k.secrets.service(n);
        let found = std::process::Command::new("security").args(["find-generic-password", "-s", &svc]).output().map(|o| o.status.success()).unwrap_or(false);
        if found {
            left += 1;
        }
    }
    check!(left == 0 && k.secrets_list().is_empty(), "Keychain items with prefix {prefix} cleaned up");
    k.shutdown();
    println!("TIMING total {} ms", t_all.elapsed().as_millis());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ macos_smoke: §18 host intelligence + host modules

#[test]
fn macos_smoke() {
    isolated("macos_smoke", Opts::default(), macos_smoke_case);
}

const HOST_V1: &str = r#"import Foundation
import AppKit

// OpenApp host module: JSON-RPC 2.0, one JSON object per line on stdin/stdout.
func reply(_ id: Any, _ result: Any?, _ error: String?) {
    var msg: [String: Any] = ["jsonrpc": "2.0", "id": id]
    if let error { msg["error"] = ["code": -32000, "message": error] } else { msg["result"] = result ?? NSNull() }
    var data = try! JSONSerialization.data(withJSONObject: msg)
    data.append(0x0A)
    FileHandle.standardOutput.write(data)
}

func handle(_ method: String, _ params: [String: Any]) throws -> Any {
    switch method {
    case "methods": return [["name": "hostname", "description": "This Mac's host name"], ["name": "frontmost", "description": "Name of the frontmost app"]]
    case "hostname": return ["name": ProcessInfo.processInfo.hostName]
    case "frontmost": return ["name": NSWorkspace.shared.frontmostApplication?.localizedName ?? ""]
    default: throw NSError(domain: "host", code: 1, userInfo: [NSLocalizedDescriptionKey: "unknown method \(method)"])
    }
}

while let line = readLine() {
    guard let obj = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any],
          let method = obj["method"] as? String else { continue }
    let id = obj["id"] ?? NSNull()
    do { reply(id, try handle(method, obj["params"] as? [String: Any] ?? [:]), nil) } catch { reply(id, nil, error.localizedDescription) }
}
"#;

async fn macos_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    check!(k.host_std.is_some(), "openapp-macos binary found: {}", k.host_std.as_ref().map(|h| h.bin.display().to_string()).unwrap_or_default());
    // the next host-module consent decision (allow / deny)
    let decision = Arc::new(Mutex::new("allow".to_string()));
    {
        let decision = decision.clone();
        auto_answer(&k, &sink, move |_| Some(decision.lock().unwrap().clone()));
    }
    let t = Instant::now();
    k.start(false).await?;
    for _ in 0..300 {
        if k.host_status.lock().unwrap()["available"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let st = k.host_status.lock().unwrap().clone();
    check!(st["available"] == true, "openapp-macos started, status in {} ms: ai={} speech={} translate={}", ms(t), st["services"]["ai"], st["services"]["speech"], st["services"]["translate"]["available"]);
    let call = |m: &'static str, p: Value| {
        let k = k.clone();
        async move { k.host_call(None, m, p, "agent").await.map_err(String::from) }
    };

    let r = call("ai.status", json!({})).await;
    check!(r.as_ref().is_ok_and(|v| v["available"] == true), "ai.status: {r:?}");
    let t = Instant::now();
    let r = call("ai.generate", json!({ "prompt": "Reply with exactly these words: on-device ok" })).await;
    check!(r.as_ref().is_ok_and(|v| v["text"].as_str().is_some_and(|s| s.to_lowercase().contains("on-device ok"))), "ai.generate plain ({} ms): {:?}", ms(t), r.map(|v| v["text"].clone()));
    let t = Instant::now();
    let schema = json!({ "type": "object", "properties": {
        "city": { "type": "string" }, "country": { "type": "string" }, "population_millions": { "type": "number" },
        "landmarks": { "type": "array", "items": { "type": "string" }, "maxItems": 3 }, "size": { "enum": ["small", "big"] } },
        "required": ["city", "country", "landmarks", "size"] });
    let r = call("ai.generate", json!({ "prompt": "Describe the capital of France.", "schema": schema })).await;
    let j = r.as_ref().map(|v| v["json"].clone()).unwrap_or_default();
    check!(j["city"].is_string() && j["landmarks"].is_array() && (j["size"] == "small" || j["size"] == "big"), "ai.generate guided JSON ({} ms, {}): {j}", ms(t), r.as_ref().map(|v| v["mode"].clone()).unwrap_or_default());

    // tts.render → speech.transcribe round trip (workspace-relative output, /workspace input)
    let t = Instant::now();
    let r = call("tts.render", json!({ "text": "OpenApp transcription test", "path": "out/tts.m4a" })).await;
    check!(r.as_ref().is_ok_and(|v| v["path"] == "out/tts.m4a") && ws.join("out/tts.m4a").is_file(), "tts.render → out/tts.m4a ({} ms): {r:?}", ms(t));
    let t = Instant::now();
    let r = call("speech.transcribe", json!({ "path": "/workspace/out/tts.m4a", "locale": "en-US" })).await;
    let text = r.as_ref().map(|v| v["text"].as_str().unwrap_or("").to_lowercase()).unwrap_or_default();
    check!(text.contains("transcription") && text.contains("test"), "speech.transcribe round trip ({} ms): {text:?} {:?}", ms(t), r.as_ref().err());

    // vision.ocr on a rendered PNG (test-only sidecar method, called on the sidecar directly)
    let png = ws.join("out/ocr.png");
    let _ = std::fs::create_dir_all(ws.join("out"));
    let sc = k.host_std.clone().expect("openapp-macos sidecar");
    let r = sc.call("debug.render_text", json!({ "text": "Invoice 2026 OpenApp", "path": png.to_string_lossy() }), Duration::from_secs(30)).await;
    check!(r.is_ok(), "rendered a PNG with text: {r:?}");
    let t = Instant::now();
    let r = call("vision.ocr", json!({ "path": "out/ocr.png" })).await;
    check!(r.as_ref().is_ok_and(|v| v["text"].as_str().is_some_and(|s| s.contains("OpenApp") && s.contains("2026"))), "vision.ocr ({} ms): {:?}", ms(t), r.map(|v| v["text"].clone()));

    let t = Instant::now();
    let r = call("nl.embed", json!({ "texts": ["hello world", "good morning"] })).await;
    check!(r.as_ref().is_ok_and(|v| v["vectors"].as_array().is_some_and(|a| a.len() == 2 && a[0].as_array().is_some_and(|x| x.len() == 512))), "nl.embed: 2 × 512 ({} ms)", ms(t));
    let r = call("nl.language", json!({ "text": "Bonjour tout le monde, comment allez-vous ?" })).await;
    check!(r.as_ref().is_ok_and(|v| v["language"] == "fr"), "nl.language: {:?}", r.map(|v| v["language"].clone()));
    let t = Instant::now();
    let r = call("translate", json!({ "text": "Good morning", "from": "en", "to": "de" })).await;
    println!("INFO translate en→de ({} ms): {}", ms(t), r.as_ref().map(|v| v["text"].to_string()).unwrap_or_else(|e| e.clone()));
    let r = call("speech.transcribe", json!({ "path": "/etc/hosts" })).await;
    check!(r.as_ref().is_err_and(|e| e.contains("outside the workspace")), "paths outside the workspace are refused: {r:?}");
    let research = std::fs::read_to_string(k.state_dir.join("research.jsonl")).unwrap_or_default();
    check!(research.contains("\"host_service\"") && !research.contains("on-device ok") && !research.contains("Invoice"), "research log: host_service entries without content");

    // HTTP endpoint (what module servers use)
    let ep = k.mcp.get().unwrap();
    let base = format!("http://127.0.0.1:{}/host", ep.port);
    let token = k.host_token("module:smoke");
    let post = |path: String, tok: String, body: Value| async move {
        let out = tokio::process::Command::new("curl")
            .args(["-sS", "-m", "60", "-w", "\n%{http_code}", "-X", "POST", &path, "-H", &format!("Authorization: Bearer {tok}"), "-H", "Content-Type: application/json", "-d", &body.to_string()])
            .output()
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        let (b, c) = s.rsplit_once('\n').unwrap_or((&s, ""));
        (c.to_string(), serde_json::from_str::<Value>(b).unwrap_or(Value::Null))
    };
    let (code, v) = post(format!("{base}/nl.language"), token.clone(), json!({ "text": "Hello there, how are you today?" })).await;
    check!(code == "200" && v["language"] == "en", "POST /host/nl.language with a module token → {code} {v}");
    let (code, _) = post(format!("{base}/nl.language"), "wrong".into(), json!({ "text": "x" })).await;
    check!(code == "401", "POST /host with a wrong token → {code}");

    // audio.record with transcribe "apple" + insert: no module server needed
    let m = ws.join("modules/dict");
    std::fs::create_dir_all(m.join("views"))?;
    std::fs::write(m.join("openapp.json"), r#"{ "id": "dict", "name": "Dictation", "views": { "button": { "kind": "spec", "file": "views/button.json" } },
        "placements": [ { "view": "button", "anchor": "composer.actions" } ] }"#)?;
    std::fs::write(m.join("views/button.json"), r#"{ "type": "icon-button", "icon": "mic", "label": "Dictate", "action": { "host": "audio.record", "transcribe": "apple", "insert": true } }"#)?;
    let lv = k.module_load("dict").await;
    check!(lv.is_ok(), "dictation module (spec view only) loaded");
    std::fs::write(k.state_dir.join("grants.json"), r#"{ "microphone": { "dict": true } }"#)?;
    let pid = k.snapshot().layout.placements.iter().find(|p| p.module == "dict").map(|p| p.id.clone()).unwrap();
    let audio = std::fs::read(ws.join("out/tts.m4a"))?;
    let t = Instant::now();
    let r = openapp_lib::primitives::audio::save(&k, &pid, json!(audio.iter().map(|b| *b as u64).collect::<Vec<_>>()), "audio/mp4", None, Some(1800)).await;
    let ins = sink.all("oa://composer").last().cloned().unwrap_or_default();
    check!(r.is_ok() && ins["insert"].as_str().is_some_and(|s| s.to_lowercase().contains("transcription")), "audio.record transcribe apple + insert ({} ms): {}", ms(t), ins["insert"]);
    let r = openapp_lib::primitives::audio::save(&k, &pid, json!([26, 69, 223, 163, 1, 0, 0, 0]), "audio/webm;codecs=opus", None, None).await;
    check!(r.as_ref().is_err_and(|e| e.contains("cannot decode") || e.contains("mp4")), "webm with transcribe apple → clear error: {:?}", r.err());

    // §19.1 presentations
    let r = k.ui_action_value("chat", json!({ "host": "view.open", "view": "dict/button", "as": "sheet", "title": "Dictation" }), None).await;
    let pr = k.snapshot().presented;
    check!(r.is_ok() && pr.len() == 1 && pr[0]["as"] == "sheet", "view.open sheet → presented {:?}", pr);
    let ppid = format!("present:{}", pr[0]["id"].as_str().unwrap());
    check!(k.placement_view(&ppid) == Some(("dict".into(), "button".into())), "presentation placement id resolves to its module view");
    check!(k.ui_action_value("chat", json!({ "host": "view.open", "view": "dict/nope" }), None).await.is_err(), "view.open of a missing view fails");
    k.ui_action_value("chat", json!({ "host": "view.close" }), None).await.map_err(anyhow::Error::msg)?;
    check!(k.snapshot().presented.is_empty(), "view.close closes the top presentation");
    let r = k.ui_action_value("chat", json!({ "host": "view.open", "view": "dict/button", "as": "tab", "region": "tools", "title": "Dictate" }), None).await;
    let snap = k.snapshot();
    check!(r.is_ok() && snap.layout.regions.iter().any(|r| r.id == "tools" && r.mode == "tabs") && snap.layout.placements.iter().any(|p| p.anchor == "region:tools"), "view.open as tab creates the region and places the view");

    // §18.1 host module: consent card → swiftc build → call
    let hm = ws.join("modules/hostinfo");
    std::fs::create_dir_all(hm.join("host"))?;
    std::fs::write(hm.join("host/main.swift"), HOST_V1)?;
    std::fs::write(hm.join("openapp.json"), r#"{ "id": "hostinfo", "name": "Host info", "host": { "swift": "host/main.swift", "summary": "Reads this Mac's host name and the frontmost app's name." } }"#)?;
    let t = Instant::now();
    let r = k.module_load("hostinfo").await;
    let card = sink.cards().into_iter().find(|c| c["title"].as_str().is_some_and(|t| t.contains("run code on your Mac"))).unwrap_or_default();
    check!(card["details"]["imports"] == json!(["AppKit", "Foundation"]) && card["diff"].as_str().is_some_and(|d| d.contains("+import AppKit")), "consent card: imports {} + diff ({} lines)", card["details"]["imports"], card["diff"].as_str().map(|d| d.lines().count()).unwrap_or(0));
    check!(r.as_ref().is_ok_and(|v| v["host_part"]["running"] == true), "host module approved, compiled and started ({} ms): methods {}", ms(t), r.as_ref().map(|v| v["host_part"]["methods"].to_string()).unwrap_or_else(|e| e.to_string()));
    let t = Instant::now();
    let r = k.host_call(Some("hostinfo"), "hostname", json!({}), "agent").await.map_err(String::from);
    check!(r.as_ref().is_ok_and(|v| v["name"].as_str().is_some_and(|s| !s.is_empty())), "host_call hostinfo/hostname ({} ms): {r:?}", ms(t));
    let (code, v) = post(format!("{base}/hostinfo/frontmost"), ep.token.clone(), json!({})).await;
    check!(code == "200" && v["name"].is_string(), "POST /host/hostinfo/frontmost → {code} {v}");
    let t = Instant::now();
    k.module_load("hostinfo").await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    check!(sink.cards().len() == 1, "reload with the same source: no new card, cached build ({} ms)", ms(t));
    // a compile error comes back structured (after consent for the new source)
    std::fs::write(hm.join("host/main.swift"), HOST_V1.replace("ProcessInfo.processInfo.hostName", "ProcessInfo.processInfo.noSuchProperty"))?;
    let r = k.module_load("hostinfo").await;
    let e = r.err().unwrap_or_default();
    check!(e["diagnostics"][0]["file"] == "modules/hostinfo/host/main.swift" && e["diagnostics"][0]["line"].as_u64().is_some(), "compile error returned structured: {}", e["diagnostics"][0]);
    // deny: the changed source does not run
    *decision.lock().unwrap() = "deny".into();
    std::fs::write(hm.join("host/main.swift"), HOST_V1.replace("This Mac's host name", "Host name v3"))?;
    let r = k.module_load("hostinfo").await;
    let card = sink.cards().last().cloned().unwrap_or_default();
    check!(r.as_ref().is_err_and(|e| e["error"].as_str().is_some_and(|s| s.contains("denied"))) && card["diff"].as_str().is_some_and(|d| d.contains("-") && d.contains("+")), "changed source → new card with diff → denied → not loaded");
    let g: Value = serde_json::from_slice(&std::fs::read(k.state_dir.join("grants.json"))?)?;
    check!(g["host_modules"]["hostinfo"]["allowed"] == false, "grants.json mirrors the host-module decision");

    k.shutdown();
    println!("TIMING total {} s", t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ channels_host: §21 G1 live channels + G9 lifecycle (host part)

#[test]
fn channels_host() {
    isolated("channels_host", Opts { env: CHANNELS_ENV, ..Opts::default() }, channels_host_case);
}

/// Host module: publishes host:hostpub/tick on start, subscribes host:hostpub/in and echoes it on host:hostpub/echo.
const CHANNELS_HOST_SWIFT: &str = r#"import Foundation

func send(_ msg: [String: Any]) {
    var data = try! JSONSerialization.data(withJSONObject: msg)
    data.append(0x0A)
    FileHandle.standardOutput.write(data)
}
func notify(_ method: String, _ params: [String: Any]) { send(["jsonrpc": "2.0", "method": method, "params": params]) }

while let line = readLine() {
    guard let obj = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any],
          let method = obj["method"] as? String else { continue }
    let params = obj["params"] as? [String: Any] ?? [:]
    if method == "channel.message" {
        notify("channel.publish", ["topic": "host:hostpub/echo", "data": ["echo": params["data"] ?? NSNull()]])
        continue
    }
    guard let id = obj["id"] else { continue }
    if method == "methods" {
        send(["jsonrpc": "2.0", "id": id, "result": [["name": "ping", "description": "pong"]]])
        notify("channel.subscribe", ["topic": "host:hostpub/in", "replay": false])
        notify("channel.publish", ["topic": "host:hostpub/tick", "data": ["n": 41]])
        notify("channel.publish", ["topic": "host:hostpub/tick", "data": ["n": 42]])
        notify("channel.publish", ["topic": "viewmod/stolen", "data": 1])
    } else {
        send(["jsonrpc": "2.0", "id": id, "result": ["pong": true]])
    }
}
"#;
async fn post(url: &str, token: &str, body: Value) -> (u16, Value) {
    let c = reqwest::Client::new();
    match c.post(url).bearer_auth(token).json(&body).send().await {
        Ok(r) => {
            let s = r.status().as_u16();
            (s, r.json().await.unwrap_or(Value::Null))
        }
        Err(e) => (0, json!(e.to_string())),
    }
}

async fn channels_host_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    let (k, sink) = fixtures::channels_kernel(&ws).await?;

    // ---- G9 lazy activation ----
    check!(status(&k, "lazy") == "idle", "lazy module without a placement is idle after start (status {})", status(&k, "lazy"));
    let lazy_dir = ws.join("modules/lazy").canonicalize()?;
    check!(!openapp_lib::modules::host_pids().lock().unwrap().contains_key(&lazy_dir), "lazy module: no server process yet");
    check!(wait_for(10, || status(&k, "viewmod") == "running").await, "module with a visible placement (onView) is running after start");
    let d = k.describe();
    check!(d["lifecycle"]["modules"]["lazy"]["activation"] == json!(["onView", "onAction"]) && d["primitives"].as_array().is_some_and(|p| p.iter().any(|x| x["name"] == "channels") && p.iter().any(|x| x["name"] == "lifecycle")),
        "describe: lifecycle (default activation) and primitives list ({} primitives)", d["primitives"].as_array().map(|a| a.len()).unwrap_or(0));
    let t = Instant::now();
    let r = k.call_module_tool("lazy", "ping", Default::default()).await;
    check!(r.as_ref().is_ok_and(|r| text_of(r).starts_with("pong")) && status(&k, "lazy") == "running", "first action activates the idle module ({} ms): {:?}", t.elapsed().as_millis(), r.as_ref().map(text_of));

    // ---- G9 resources (host) ----
    check!(wait_for(10, || lifecycle::resources(&k).get("lazy").is_some_and(|r| r["rss_mb"].as_f64().unwrap_or(0.0) > 1.0 && r["status"] == "running")).await,
        "resources: lazy {}", lifecycle::resources(&k).get("lazy").cloned().unwrap_or_default());
    let hd = k.snapshot().host_data;
    check!(hd["resources"].as_array().is_some_and(|a| a.iter().any(|i| i["id"] == "lazy" && i["icon"] == "cpu")) && hd["safe_mode"] == false, "host data resources + safe_mode in the snapshot");

    // ---- G9 idle stop ----
    let t = Instant::now();
    check!(wait_for(15, || status(&k, "lazy") == "idle").await, "idle stop after OPENAPP_IDLE_SECS=3 ({} ms); viewmod (visible) still {}", t.elapsed().as_millis(), status(&k, "viewmod"));
    check!(status(&k, "viewmod") == "running", "visible module is not idle-stopped");
    let r = k.call_module_tool("lazy", "ping", Default::default()).await;
    check!(r.is_ok() && status(&k, "lazy") == "running", "idle module starts again on the next action");

    // ---- G1 kernel topics, $stream, view subscriptions ----
    let mut ks = channels::subscribe(&k, "kernel/sessions", false);
    let mut kn = channels::subscribe(&k, "kernel/notify", false);
    let sid = k.active_session();
    k.session_rename(&sid, "Channels smoke").map_err(anyhow::Error::msg)?;
    k.notify("hello channels");
    let s1 = tokio::time::timeout(Duration::from_secs(3), ks.recv()).await.ok().flatten();
    let n1 = tokio::time::timeout(Duration::from_secs(3), kn.recv()).await.ok().flatten();
    check!(s1.is_some_and(|m| m.json().is_some_and(|v| v["sessions"].is_array())) && n1.is_some_and(|m| m.json() == Some(&json!({ "text": "hello channels" }))), "kernel/sessions and kernel/notify published");

    let (text0, _, problems) = k.snapshot_text();
    check!(text0.contains("no count") && problems.is_empty(), "$stream default shown in openapp_snapshot, no view problems");
    channels::publish(&k, "viewmod/counter", ChannelData::Json(json!({ "n": 7 })));
    let (text1, _, _) = k.snapshot_text();
    check!(text1.contains("\"7\"") || text1.contains(" 7"), "$stream resolves the latest message in openapp_snapshot");
    let placement = k.snapshot().layout.placements.iter().find(|p| p.module == "viewmod").map(|p| p.id.clone()).expect("viewmod placement");
    let sub = channels::view_subscribe(&k, &placement, "viewmod/counter").map_err(anyhow::Error::msg)?;
    check!(sub["replay"].as_array().is_some_and(|a| a.len() == 1 && a[0]["data"]["n"] == 7), "view subscribe: ring replay {}", sub["replay"]);
    channels::publish(&k, "viewmod/counter", ChannelData::Json(json!({ "n": 8 })));
    check!(wait_for(3, || sink.all("oa://channel").iter().any(|m| m["topic"] == "viewmod/counter" && m["data"]["n"] == 8 && m["sub"] == sub["sub"])).await, "live message → oa://channel with sub id");
    channels::view_unsubscribe(&k, sub["sub"].as_u64().unwrap());
    let r = channels::view_publish(&k, &placement, &json!({ "topic": "lazy/x", "data": 1 }));
    check!(r.as_ref().is_err_and(|e| e.contains("may not publish")), "ownership: a view may not publish on another module's topic: {r:?}");
    let r = channels::view_subscribe(&k, &placement, "lazy/secret");
    check!(r.as_ref().is_err_and(|e| e.contains("public")), "ownership: subscribing to a non-public topic of another module is denied");

    // ---- POST /host/channel.publish ----
    let ep = k.mcp.get().unwrap();
    let base = format!("http://127.0.0.1:{}/host", ep.port);
    let tok = k.host_token("module:lazy");
    let mut lr = channels::subscribe(&k, "lazy/news", false);
    let (code, v) = post(&format!("{base}/channel.publish"), &tok, json!({ "topic": "lazy/news", "data": { "hi": 1 } })).await;
    let got = tokio::time::timeout(Duration::from_secs(3), lr.recv()).await.ok().flatten();
    check!(code == 200 && got.is_some_and(|m| m.json() == Some(&json!({ "hi": 1 }))), "POST /host/channel.publish on own topic → {code} {v}, delivered");
    let (code, v) = post(&format!("{base}/channel.publish"), &tok, json!({ "topic": "viewmod/counter", "data": 0 })).await;
    check!(code >= 400 && v["error"].as_str().is_some_and(|e| e.contains("may not publish")), "POST /host/channel.publish on another module's topic → {code} {}", v["error"]);

    // ---- host module over JSON-RPC notifications ----
    write_module(&ws, "hostpub", json!({ "id": "hostpub", "name": "Host pub", "host": { "swift": "host/main.swift", "summary": "Publishes test messages on live channels." },
        "views": { "tick": { "kind": "spec", "file": "views/tick.json" } }, "placements": [ { "view": "tick", "anchor": "composer.above" } ] }),
        &[("host/main.swift", CHANNELS_HOST_SWIFT), ("views/tick.json", r#"{ "type": "text", "value": { "$stream": "host:hostpub/tick", "path": "n", "default": "-" } }"#)])?;
    let mut tick = channels::subscribe(&k, "host:hostpub/tick", true);
    let t = Instant::now();
    let r = k.module_load("hostpub").await;
    check!(r.is_ok(), "host module compiled and started ({} ms){}", t.elapsed().as_millis(), r.as_ref().err().map(|e| format!(": {e}")).unwrap_or_default());
    let mut ticks = Vec::new();
    while ticks.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(5), tick.recv()).await {
            Ok(Some(m)) => ticks.push(m.json().cloned().unwrap_or_default()),
            _ => break,
        }
    }
    check!(ticks == vec![json!({ "n": 41 }), json!({ "n": 42 })], "host module publishes over JSON-RPC (channel.publish): {ticks:?}");
    check!(channels::hub(&k).ring("viewmod/stolen").is_empty(), "host module may not publish on another module's topic");
    let subscribed = wait_for(5, || k.describe()["channels"]["topics"].as_array().is_some_and(|a| a.iter().any(|t| t["topic"] == "host:hostpub/in" && t["subscribers"].as_u64() > Some(0)))).await;
    let mut echo = channels::subscribe(&k, "host:hostpub/echo", false);
    channels::publish(&k, "host:hostpub/in", ChannelData::Json(json!({ "ping": 1 })));
    let e = tokio::time::timeout(Duration::from_secs(5), echo.recv()).await.ok().flatten();
    check!(subscribed && e.is_some_and(|m| m.json() == Some(&json!({ "echo": { "ping": 1 } }))), "host module subscribes (channel.subscribe) and gets channel.message → echo");
    let (text2, _, _) = k.snapshot_text();
    check!(text2.contains("42"), "$stream on a host-module topic in openapp_snapshot");

    // ---- /safe round trip ----
    k.send_prompt("/safe".into());
    check!(wait_for(10, || lifecycle::safe_mode(&k) && !k.snapshot().modules.iter().any(|m| m.status == "running")).await, "/safe: safe mode on, no module running");
    let s = k.snapshot();
    check!(s.layout.placements.is_empty() && s.layout.regions.is_empty() && s.host_data["safe_mode"] == true, "/safe: layout hidden (kernel chrome only)");
    check!(k.module_load("lazy").await.is_err_and(|e| e["error"].as_str().is_some_and(|x| x.contains("safe mode"))), "/safe: module_load is refused");
    let k2 = Kernel::new_opts(ws.clone(), None, None, Arc::new(openapp_lib::kernel::NullSink))?;
    check!(lifecycle::safe_mode(&k2) && k2.snapshot().layout.placements.is_empty(), "safe mode is persisted (a new kernel starts in safe mode)");
    drop(k2);
    k.send_prompt("/safe off".into());
    check!(wait_for(20, || !lifecycle::safe_mode(&k) && status(&k, "viewmod") == "running" && !k.snapshot().layout.placements.is_empty()).await, "/safe off: modules start again, layout back ({} placements)", k.snapshot().layout.placements.len());
    check!(status(&k, "lazy") == "idle", "/safe off: lazy module back to idle (status {})", status(&k, "lazy"));
    k.shutdown();
    println!("TIMING total {} s (host part)", t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ wave2_host: §21 wave 2 (host part)

#[test]
fn wave2_host() {
    isolated("wave2_host", Opts { env: WAVE2_ENV, keychain: true, ..Opts::default() }, wave2_host_case);
}

/// G8: a host-module service on the kernel runtime: meter ticks at 10 Hz, a plugin window, its pid.
const WAVE2_HOST_SWIFT: &str = r#"import AppKit
import Foundation

nonisolated(unsafe) var ticks = 0
let timer = Timer(timeInterval: 0.1, repeats: true) { _ in
    ticks += 1
    OpenAppHost.publish("host:daw/meters", ["n": ticks, "l": 0.25, "r": 0.5])
}
RunLoop.main.add(timer, forMode: .common)
OpenAppHost.method("open_plugin", "Open the plugin window") { _ in
    let v = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 200))
    let id = OpenAppHost.openWindow(title: "Plugin", width: 300, height: 200, content: v, activate: false)
    return ["window": id, "count": OpenAppHost.visibleWindows().count]
}
OpenAppHost.method("close_plugin", "Close the plugin window") { p in
    OpenAppHost.closeWindow(p["window"] as? String ?? "")
    return ["count": OpenAppHost.visibleWindows().count]
}
OpenAppHost.method("pid") { _ in ["pid": Int(ProcessInfo.processInfo.processIdentifier)] }
OpenAppHost.run()
"#;
fn job(k: &Kernel, id: &str) -> Value {
    jobs::list(k).into_iter().find(|j| j["id"] == id).unwrap_or_default()
}

/// Drain kernel/jobs messages.
fn drain(rx: &mut channels::Receiver, out: &mut Vec<Value>) {
    while let Some(m) = rx.try_recv() {
        out.push(m.json().cloned().unwrap_or_default());
    }
}

async fn wave2_host_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    let state = openapp_lib::state::state_dir(&ws);
    std::fs::create_dir_all(&state)?;
    // ---- modules on disk before start ----
    write_module(&ws, "jobber", json!({ "id": "jobber", "name": "Jobber", "server": { "command": ["node", "server.ts"] },
        "jobs": [
            { "name": "every", "schedule": { "every": "1s" }, "action": { "tool": "tick" } },
            { "name": "cron", "schedule": "*/2 * * * * *", "action": { "tool": "tick" } },
            { "name": "watch", "schedule": { "watch": "inbox/*.txt" }, "action": { "tool": "echo" } },
            { "name": "hook", "schedule": { "webhook": true }, "action": { "tool": "echo" } },
            { "name": "hourly", "schedule": { "every": "1h" }, "action": { "tool": "tick" } }
        ] }), &[("server.ts", WAVE2_SERVER_TS)])?;
    // a 3 h gap since the last hourly run (asleep / OpenApp closed)
    let three_h = (chrono::Local::now() - chrono::Duration::hours(3)).to_rfc3339();
    std::fs::write(state.join("jobs.json"), json!({ "runtime": {}, "state": { "jobber/hourly": { "created": three_h, "last_run": three_h, "runs": 4 } } }).to_string())?;
    let (k, sink) = fixtures::wave2_kernel(&ws)?;
    let mut kj = channels::subscribe(&k, "kernel/jobs", false);
    let mut msgs: Vec<Value> = Vec::new();
    k.start(false).await?;

    // ---- G6 jobs ----
    let ok = wait_for(15, || {
        drain(&mut kj, &mut msgs);
        msgs.iter().any(|m| m["op"] == "start" && m["job"] == "jobber/hourly" && m["trigger"] == "missed")
    })
    .await;
    check!(ok, "missed run: the hourly job overdue by 3 h runs once on start (trigger \"missed\")");
    let activated = wait_for(10, || status(&k, "jobber") == "running" && std::fs::read_to_string(state.join("research.jsonl")).is_ok_and(|t| t.lines().any(|l| l.contains("\"activate\"") && l.contains("onJob") && l.contains("module:jobber")))).await;
    check!(activated, "G9 onJob: the lazy module was started by its first job run");
    let t = Instant::now();
    let ok = wait_for(15, || job(&k, "jobber/every")["runs"].as_u64() >= Some(3) && job(&k, "jobber/cron")["runs"].as_u64() >= Some(2)).await;
    check!(ok && job(&k, "jobber/every")["last_status"] == "ok", "every 1s ran {} times, cron */2s ran {} times ({} ms)", job(&k, "jobber/every")["runs"], job(&k, "jobber/cron")["runs"], t.elapsed().as_millis());
    tokio::time::sleep(Duration::from_millis(600)).await;
    drain(&mut kj, &mut msgs);
    let hourly_runs = msgs.iter().filter(|m| m["op"] == "start" && m["job"] == "jobber/hourly").count();
    check!(hourly_runs == 1 && job(&k, "jobber/hourly")["runs"] == 5, "missed slots are not replayed: hourly started {hourly_runs} time(s), next {}", job(&k, "jobber/hourly")["next"]);
    let end = msgs.iter().find(|m| m["op"] == "end" && m["job"] == "jobber/every").cloned().unwrap_or_default();
    check!(end["status"] == "ok" && end["output_tail"].as_str().is_some_and(|o| o.starts_with("tick")) && end["ms"].is_u64(), "kernel/jobs end message: {end}");
    let logf = state.join("jobs").join("jobber.every.jsonl");
    check!(std::fs::read_to_string(&logf).is_ok_and(|t| t.contains("\"op\":\"start\"") && t.contains("\"op\":\"end\"")), "run log in the state dir ({})", logf.display());
    let research = std::fs::read_to_string(state.join("research.jsonl")).unwrap_or_default();
    check!(research.contains("\"job_run\""), "job runs in the research log");
    // watch
    let t = Instant::now();
    std::fs::write(ws.join("inbox/a.txt"), "hello")?;
    let ok = wait_for(10, || {
        drain(&mut kj, &mut msgs);
        msgs.iter().any(|m| m["op"] == "end" && m["job"] == "jobber/watch" && m["output_tail"].as_str().is_some_and(|o| o.contains("inbox/a.txt")))
    })
    .await;
    check!(ok, "watch job fired on a file change ({} ms)", t.elapsed().as_millis());
    std::fs::write(ws.join("inbox/ignored.md"), "x")?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    check!(job(&k, "jobber/watch")["runs"] == 1, "watch ignores files outside the glob");
    // webhook
    let url = job(&k, "jobber/hook")["url"].as_str().unwrap_or("").to_string();
    let r = reqwest::Client::new().post(&url).json(&json!({ "hello": "webhook" })).send().await?;
    let code = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or_default();
    check!(code == 200 && v["status"] == "ok" && v["output_tail"].as_str().is_some_and(|o| o.contains("webhook")) && v["trigger"] == "webhook", "webhook job via HTTP {url} → {code} {}", v["output_tail"]);
    let bad = reqwest::Client::new().post(url.replace("/hook/wh_", "/hook/wh_x")).send().await?.status().as_u16();
    check!(bad == 404, "unknown webhook token → {bad}");
    // runtime job API
    let c = Caller::new("agent");
    let r = primitives::call_action(&k, "job.create", json!({ "module": "jobber", "name": "rt", "schedule": { "every": "1h" }, "action": { "tool": "tick" } }), c.clone()).await.unwrap();
    check!(r.is_ok(), "job.create (runtime): {:?}", r.as_ref().map(|v| v["job"]["next"].clone()));
    let r = primitives::call_action(&k, "job.run_now", json!({ "id": "jobber/rt" }), c.clone()).await.unwrap();
    check!(r.as_ref().is_ok_and(|v| v["status"] == "ok" && v["trigger"] == "manual"), "job.run_now → {:?}", r.as_ref().map(|v| v["output_tail"].clone()));
    let r = primitives::call_action(&k, "job.create", json!({ "name": "x", "schedule": "nope" }), Caller::new("module:jobber")).await.unwrap();
    check!(r.is_err_and(|e| e.contains("cron")), "a bad schedule is refused with a clear error");
    let r = primitives::call_action(&k, "job.delete", json!({ "module": "jobber", "name": "every" }), Caller::new("module:other")).await.unwrap();
    check!(r.is_err_and(|e| e.contains("own jobs")), "a module may only manage its own jobs");
    let hd = k.snapshot().host_data;
    check!(hd["jobs"].as_array().is_some_and(|a| a.len() == 6 && a.iter().any(|j| j["id"] == "jobber/hook" && j["url"].is_string())), "host data jobs ({} items)", hd["jobs"].as_array().map(|a| a.len()).unwrap_or(0));
    check!(k.describe()["jobs"]["jobs"].as_array().is_some_and(|a| a.len() == 6) && k.describe()["primitives"].as_array().is_some_and(|p| ["jobs", "background", "secret_release", "host_service", "agents"].iter().all(|n| p.iter().any(|x| x["name"] == *n))), "describe: jobs + the five wave-2 primitives");

    // ---- G6 background ----
    let reasons = background::reasons(&k);
    check!(background::keep_running(&k) && reasons.iter().any(|r| r.contains("job")), "keep running with the window closed: {reasons:?}");
    let r = primitives::call_action(&k, "app.login_item", json!({}), Caller::new("user")).await.unwrap();
    check!(r.as_ref().is_ok_and(|v| v["status"].as_str().is_some_and(|s| ["enabled", "not_registered", "requires_approval", "not_found"].contains(&s) || s.starts_with("unavailable"))), "app.login_item status (SMAppService, not changed): {:?}", r.map(|v| v["status"].clone()));

    // ---- G7 secret.generate ----
    let r = primitives::call_action(&k, "secret.generate", json!({ "name": "wave2d-key", "kind": "ed25519", "label": "Deploy key" }), c.clone()).await.unwrap();
    let pubkey = r.as_ref().map(|v| v["public_openssh"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    let pf = ws.with_file_name("wave2d-key.pub");
    std::fs::write(&pf, format!("{pubkey}\n"))?;
    let fp = std::process::Command::new("ssh-keygen").arg("-l").arg("-f").arg(&pf).output()?;
    let _ = std::fs::remove_file(&pf);
    check!(fp.status.success() && String::from_utf8_lossy(&fp.stdout).contains("ED25519"), "secret.generate ed25519 → public key ({})", String::from_utf8_lossy(&fp.stdout).trim());
    let (meta, value) = k.secrets.get("wave2d-key").map_err(anyhow::Error::msg)?;
    check!(meta["kind"] == "keypair" && String::from_utf8_lossy(&value).starts_with("-----BEGIN PRIVATE KEY-----") && !r.unwrap().to_string().contains("PRIVATE"), "private key only in the Keychain (result carries the public part only)");
    let r = primitives::call_action(&k, "secret.generate", json!({ "name": "wave2d-pw", "kind": "password", "length": 20 }), c.clone()).await.unwrap();
    check!(r.as_ref().is_ok_and(|v| v["length"] == 20 && v.get("value").is_none()), "secret.generate password (value not returned)");

    // ---- G8 host-module service with a window ----
    write_module(&ws, "daw", json!({ "id": "daw", "name": "DAW", "host": { "swift": "host/main.swift", "summary": "Runs the audio engine on your Mac and shows plugin windows.", "service": true, "windows": true } }), &[("host/main.swift", WAVE2_HOST_SWIFT)])?;
    let mut meters = channels::subscribe(&k, "host:daw/meters", false);
    let t = Instant::now();
    let r = k.module_load("daw").await;
    check!(r.is_ok(), "host-module service compiled with the OpenAppHost runtime and started ({} ms){}", t.elapsed().as_millis(), r.as_ref().err().map(|e| format!(": {e}")).unwrap_or_default());
    let card = sink.all("oa://chat").into_iter().find(|p| p["item"]["kind"] == "permission" && p["item"]["title"].as_str().is_some_and(|t| t.contains("DAW"))).map(|p| p["item"]["text"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    check!(card.contains("opens its own windows") && card.contains("Keeps running"), "consent card declares windows + service: {}", card.replace('\n', " | "));
    let mut n = 0;
    let end = Instant::now() + Duration::from_secs(5);
    while n < 5 && Instant::now() < end {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(2), meters.recv()).await {
            n += 1;
        }
    }
    check!(n >= 5, "host service publishes meter ticks on host:daw/meters ({n} received)");
    let w = k.host_call(Some("daw"), "open_plugin", json!({}), "agent").await.map_err(String::from);
    let wid = w.as_ref().map(|v| v["window"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    let cnt = k.host_call(Some("daw"), "openapp.windows", json!({}), "agent").await.map_err(String::from);
    check!(w.as_ref().is_ok_and(|v| v["count"] == 1) && cnt.as_ref().is_ok_and(|v| v["count"] == 1), "AppKit window opened (own report: {:?})", cnt);
    let cl = k.host_call(Some("daw"), "close_plugin", json!({ "window": wid }), "agent").await.map_err(String::from);
    check!(cl.as_ref().is_ok_and(|v| v["count"] == 0), "window closed (count {:?})", cl.map(|v| v["count"].clone()));
    let pid1 = k.host_call(Some("daw"), "pid", json!({}), "agent").await.ok().and_then(|v| v["pid"].as_i64()).unwrap_or(0);
    let _ = std::process::Command::new("kill").args(["-9", &pid1.to_string()]).status();
    let mut pid2 = 0i64;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        pid2 = k.lock().modules.get("daw").and_then(|m| m.host.clone()).and_then(|h| h.pid()).unwrap_or(0) as i64;
        if pid2 != 0 && pid2 != pid1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ok = pid2 != 0 && pid2 != pid1;
    while meters.try_recv().is_some() {}
    let tick = tokio::time::timeout(Duration::from_secs(5), meters.recv()).await.ok().flatten();
    check!(ok && tick.is_some(), "supervisor restarted the killed host service (pid {pid1} → {pid2}) and ticks resumed");
    check!(background::reasons(&k).iter().any(|r| r.contains("Mac service")), "host service counts as a background reason");

    // ---- G10 agent module (on-device) ----
    write_module(&ws, "calc", json!({ "id": "calc", "name": "Calc", "server": { "command": ["node", "server.ts"] },
        "agents": [{ "name": "adder", "instructions": "AGENT.md", "tools": ["calc/add"], "model": "on-device", "budget": { "turns": 4, "minutes": 3 }, "triggers": ["nightly"] }],
        "jobs": [{ "name": "nightly", "schedule": { "webhook": true } }] }),
        &[("server.ts", WAVE2_SERVER_TS), ("AGENT.md", "You are a calculator agent. To add numbers you must call the add tool with arguments a and b; never add in your head. When the tool has returned the sum, give the final answer with just the number.")])?;
    k.module_load("calc").await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let ctx = agents::RunCtx { module: "calc".into(), agent: "adder".into(), allowed: vec!["calc/add".into()] };
    let r = agents::call_gated(&k, &ctx, "test", "calc/secret", json!({})).await;
    check!(r.as_ref().is_err_and(|e| e.starts_with("refused")), "a disallowed tool is refused: {:?}", r.err());
    let r = agents::call_gated(&k, &ctx, "test", "calc/add", json!({ "a": 2, "b": 3 })).await;
    check!(r.as_ref().is_ok_and(|v| v["text"] == "5"), "the allowed tool runs through the gate");
    let tools = agents::resolve_tools(&k, &["calc/add".into()]).await.map_err(anyhow::Error::msg)?;
    check!(tools.len() == 1 && tools[0].name == "calc__add", "agent tool list holds only calc/add");
    let ai = k.host_call(None, "ai.status", json!({}), "kernel").await.ok().and_then(|v| v["available"].as_bool()).unwrap_or(false);
    if std::env::var("OPENAPP_SMOKE_NO_AI").is_ok_and(|v| v == "1") || !ai {
        println!("SKIP on-device agent run (ai.status available={ai})");
    } else {
        let mut ka = channels::subscribe(&k, "kernel/agents", false);
        let t = Instant::now();
        let r = primitives::call_action(&k, "agent.run", json!({ "module": "calc", "agent": "adder", "input": "What is 17 + 25?", "wait": true }), Caller::new("user")).await.unwrap();
        let run = r.clone().unwrap_or_else(|e| json!({ "error": e }));
        let runs = k.snapshot().host_data["agent_runs"].clone();
        let steps = runs[0]["steps"].as_array().cloned().unwrap_or_default();
        check!(run["status"] == "ok" && run["text"].as_str().is_some_and(|t| t.contains("42")) && steps.iter().any(|s| s["tool"] == "calc/add" && s["ok"] == true) && run["turns"].as_u64() >= Some(2),
            "on-device agent: 2-step task with its one tool ({} ms, {} turns): {} · steps {}", t.elapsed().as_millis(), run["turns"], run["text"], json!(steps));
        let mut ops = Vec::new();
        while let Some(m) = ka.try_recv() {
            ops.push(m.json().and_then(|v| v["op"].as_str().map(String::from)).unwrap_or_default());
        }
        check!(ops.first().map(String::as_str) == Some("start") && ops.contains(&"step".to_string()) && ops.last().map(String::as_str) == Some("result"), "kernel/agents: {ops:?}");
        check!(k.snapshot().host_data["agents"].as_array().is_some_and(|a| a.iter().any(|x| x["id"] == "calc/adder" && x["status"] == "ok")), "host data agents shows the last run");
        // E9 pattern: a job triggers the agent
        let url = job(&k, "calc/nightly")["url"].as_str().unwrap_or("").to_string();
        let v: Value = reqwest::Client::new().post(&url).json(&"What is 5 + 6?").timeout(Duration::from_secs(30)).send().await?.json().await.unwrap_or_default();
        check!(v["status"] == "ok" && v["output_tail"].as_str().is_some_and(|o| o.contains("[adder]") && o.contains("11")), "job trigger runs the agent: {} · run {}", v["output_tail"], k.snapshot().host_data["agent_runs"][0]);
    }
    fixtures::wave2_cleanup(&k);
    k.shutdown();
    println!("TIMING total {} s (host part)", t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ shell_headless: agent-set native shell state

#[test]
fn shell_headless() {
    isolated("shell_headless", Opts::default(), shell_headless_case);
}

/// The agent configures tray / menus / shortcuts / dock badge / windows through `openapp_host_action` in a run
/// without an app window: each call succeeds as `{applied: false, pending: true}`, the desired state shows in
/// describe (`native_shell`), host data and the snapshot text; user-only actions are refused with what is allowed.
async fn shell_headless_case(cx: Ctx) -> anyhow::Result<()> {
    use primitives::connect::agent_action;
    let ws = cx.ws;
    write_module(
        &ws,
        "player",
        json!({ "id": "player", "name": "Player",
                "views": { "mini": { "kind": "spec", "file": "views/mini.json" }, "full": { "kind": "spec", "file": "views/full.json" } } }),
        &[("views/mini.json", r#"{ "type": "text", "value": "mini" }"#), ("views/full.json", r#"{ "type": "text", "value": "full" }"#)],
    )?;
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    k.start(false).await?;
    check!(wait_for(10, || k.lock().modules.contains_key("player")).await, "module player known");

    let tray = agent_action(&k, "tray.set", &json!({ "module": "player", "icon": "music", "title": "▶ Song", "menu": [{ "label": "Play/Pause", "action": { "tool": "toggle" } }] })).await;
    check!(tray.as_ref().is_ok_and(|v| v["ok"] == true && v["applied"] == false && v["pending"] == true), "agent tray.set headless → pending ({tray:?})");
    let menu = agent_action(&k, "menu.set", &json!({ "module": "player", "menus": [{ "title": "Player", "items": [{ "label": "Play", "action": { "tool": "play" } }] }] })).await;
    check!(menu.as_ref().is_ok_and(|v| v["pending"] == true), "agent menu.set headless → pending ({menu:?})");
    let win = agent_action(&k, "window.open", &json!({ "view": "player/full", "title": "Player", "width": 400, "height": 300 })).await;
    check!(win.as_ref().is_ok_and(|v| v["pending"] == true && v["windowId"].is_string()), "agent window.open headless → pending ({win:?})");
    let sc = agent_action(&k, "shortcut.register", &json!({ "module": "player", "accelerator": "Space", "action": { "tool": "toggle" } })).await;
    check!(sc.as_ref().is_ok_and(|v| v["pending"] == true && v["accelerator"] == "Space"), "agent shortcut.register headless → pending ({sc:?})");
    let badge = agent_action(&k, "dock.badge", &json!({ "text": "3" })).await;
    check!(badge.as_ref().is_ok_and(|v| v["pending"] == true), "agent dock.badge headless → pending ({badge:?})");

    let d = k.describe();
    let ns = &d["native_shell"];
    check!(
        ns["attached"] == false && ns["tray"]["icon"] == "music" && ns["menus"]["player"][0]["title"] == "Player" && ns["dock_badge"] == "3"
            && ns["windows"][0]["pending"] == true && ns["shortcuts"][0]["accelerator"] == "Space",
        "describe.native_shell shows the desired state: {ns}"
    );
    let snap = k.snapshot();
    check!(snap.host_data["tray"]["title"] == "▶ Song" && snap.host_data["windows"][0]["view"] == "player/full" && snap.host_data["dock"]["badge"] == "3", "snapshot hostData tray/windows/dock");
    let (text, _, _) = k.snapshot_text();
    check!(text.contains("Native shell (stored") && text.contains("tray (player)") && text.contains("opens with the app window"), "openapp_snapshot text lists the stored native state");
    let saved: Value = serde_json::from_slice(&std::fs::read(k.state_dir.join("shell.json"))?)?;
    check!(saved["tray"]["module"] == "player" && saved["menus"]["player"].is_array(), "tray + menus persisted in shell.json");
    let wid = win.as_ref().map(|v| v["windowId"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    let closed = agent_action(&k, "window.close", &json!({ "id": wid })).await;
    check!(closed.as_ref().is_ok_and(|v| v["closed"] == true) && k.describe()["native_shell"]["windows"].as_array().is_some_and(|a| a.is_empty()), "window.close of a pending window drops it ({closed:?})");

    // user-only actions are refused, and the refusal lists what the agent may run
    let r = agent_action(&k, "files.open", &json!({})).await;
    check!(r.as_ref().is_err_and(|e| e.contains("needs the user") && e.contains("tray.set") && e.contains("window.open")), "files.open refused with the allowed list ({r:?})");
    let r = agent_action(&k, "harness.select", &json!({ "id": "pi" })).await;
    check!(r.is_err(), "harness.select refused");
    let r = agent_action(&k, "safe.set", &json!({ "on": false })).await;
    check!(r.as_ref().is_err_and(|e| e.contains("safe mode off needs the user")), "safe.set off refused ({r:?})");
    let r = agent_action(&k, "model.request", &json!({ "tier": "strong" })).await;
    check!(r.as_ref().is_err_and(|e| e.contains("reason") || e.contains("no provider is running")), "model.request routes to openapp_model_request ({r:?})");
    k.shutdown();
    Ok(())
}

// ================================================================ host_plist: usage descriptions embedded in host modules

#[test]
fn host_plist() {
    isolated("host_plist", Opts::default(), host_plist_case);
}

/// CoreBluetooth at start: without an embedded NSBluetoothAlwaysUsageDescription macOS kills the process (SIGABRT).
const BLE_SWIFT: &str = r#"import Foundation
import CoreBluetooth

final class Watch: NSObject, CBCentralManagerDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {}
}
let watch = Watch()
let manager = CBCentralManager(delegate: watch, queue: DispatchQueue(label: "ble"))

func reply(_ id: Any, _ result: Any) {
    var data = try! JSONSerialization.data(withJSONObject: ["jsonrpc": "2.0", "id": id, "result": result])
    data.append(0x0A)
    FileHandle.standardOutput.write(data)
}
while let line = readLine() {
    guard let obj = (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any], let method = obj["method"] as? String else { continue }
    let id = obj["id"] ?? NSNull()
    switch method {
    case "methods": reply(id, [["name": "state", "description": "Bluetooth manager state"]])
    case "state": reply(id, ["state": manager.state.rawValue, "authorization": CBManager.authorization.rawValue, "bundle": Bundle.main.bundleIdentifier ?? ""])
    default: reply(id, NSNull())
    }
}
"#;

const BLE_PACKAGE: &str = r#"// swift-tools-version:5.9
import PackageDescription
let package = Package(name: "blepkg", platforms: [.macOS(.v14)], targets: [.executableTarget(name: "blepkg", path: "Sources")])
"#;

async fn host_plist_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let sink = Arc::new(Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    auto_answer(&k, &sink, |_| Some("allow".into()));
    k.start(false).await?;
    // `otool -P`: the (__TEXT,__info_plist) section as text (same section `otool -s __TEXT __info_plist` dumps in hex)
    let otool = |bin: &std::path::Path| {
        std::process::Command::new("otool").arg("-P").arg(bin).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
    };
    let built = |module: &str| -> Option<std::path::PathBuf> {
        support::walk(&openapp_lib::host_modules::module_root(module)).into_iter().find(|p| {
            p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(module)) && p.parent().is_some_and(|d| d.ends_with("bin") || d.ends_with("release")) && p.is_file()
        })
    };

    // swiftc path
    write_module(&ws, "ble", json!({ "id": "ble", "name": "BLE", "host": { "swift": "host/main.swift", "summary": "Reads the Bluetooth state." } }), &[("host/main.swift", BLE_SWIFT)])?;
    let t = Instant::now();
    let r = k.module_load("ble").await;
    check!(r.as_ref().is_ok_and(|v| v["host_part"]["running"] == true), "CoreBluetooth host module compiled and started ({} ms): {:?}", ms(t), r.as_ref().err());
    let bin = built("ble").unwrap_or_default();
    let plist = otool(&bin);
    check!(plist.contains("com.polygonface.openapp.host.ble") && plist.contains("NSBluetoothAlwaysUsageDescription") && plist.contains("NSLocalNetworkUsageDescription"), "otool -P (the __TEXT,__info_plist section) of {} shows the embedded plist", bin.display());
    let r = k.host_call(Some("ble"), "state", json!({}), "agent").await.map_err(String::from);
    check!(r.as_ref().is_ok_and(|v| v["state"].is_number() && v["bundle"] == "com.polygonface.openapp.host.ble"), "CBCentralManager created without SIGABRT; state answered: {r:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = k.host_call(Some("ble"), "state", json!({}), "agent").await.map_err(String::from);
    check!(r.is_ok(), "still running after the Bluetooth check: {r:?}");

    // SwiftPM path
    write_module(&ws, "blepkg", json!({ "id": "blepkg", "name": "BLE package", "host": { "package": "host", "product": "blepkg", "summary": "Reads the Bluetooth state (SwiftPM)." } }), &[("host/Package.swift", BLE_PACKAGE), ("host/Sources/main.swift", BLE_SWIFT)])?;
    let t = Instant::now();
    let r = k.module_load("blepkg").await;
    check!(r.as_ref().is_ok_and(|v| v["host_part"]["running"] == true), "SwiftPM CoreBluetooth host module built and started ({} ms): {:?}", ms(t), r.as_ref().err());
    let bin = built("blepkg").unwrap_or_default();
    check!(otool(&bin).contains("com.polygonface.openapp.host.blepkg"), "SwiftPM binary {} embeds the plist", bin.display());
    let r = k.host_call(Some("blepkg"), "state", json!({}), "agent").await.map_err(String::from);
    check!(r.as_ref().is_ok_and(|v| v["bundle"] == "com.polygonface.openapp.host.blepkg"), "SwiftPM host module answers: {r:?}");
    k.shutdown();
    println!("ALL PASS");
    Ok(())
}
