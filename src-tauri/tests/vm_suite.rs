//! VM suite (`cargo test --test vm_suite -- --ignored`): boots the app VM in a scratch VM home per case (the guest
//! image is copied from `~/.openapp/vm/images/openapp-guest-<v>.tar` or `OPENAPP_TEST_GUEST_TAR`; the home is
//! deleted afterwards) and never touches `~/.openapp/vm`. Cases run one at a time.
//! - vm_smoke: §17 bridge: Codex install + app-server, kernel MCP from the guest, egress proxy + consent cards,
//!   a module server in the VM, save_audio through the bridge, the login callback forward
//!   (OPENAPP_SMOKE_CODEX_PROXY=1: Codex's own traffic through the proxy, with a fake API key).
//! - services_smoke: §21 G2/G3 apt, supervised services, oasvc://, gateway, PTY, virtual display, qemu-user, LAN
//!   (OPENAPP_SMOKE_LAN=1 really opens the LAN listener; OPENAPP_SMOKE_LAN_PORT, default 18093).
//! - pi_smoke: §11/§12.4 Pi in the VM (OPENAPP_SMOKE_PI_FAKE_KEY=0 / OPENAPP_SMOKE_PI_TOOLS=0 skip parts).
//! - channels_vm, wave2_vm: the VM halves of the channels and wave-2 cases in `kernel_host.rs`.
//! - bulk_vm: 3 MiB up/down through the HTTP CONNECT and SOCKS5 routes and a 1 MiB MCP request (the vsock relay).

#[macro_use]
mod support;

use openapp_lib::kernel::Kernel;
use openapp_lib::primitives::channels::{self, ChannelData};
use openapp_lib::primitives::{self as prims, Caller, lifecycle};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::fixtures::{self, CHANNELS_ENV, CHANNELS_SERVER_TS, WAVE2_ENV, WAVE2_SERVER_TS};
use support::{Ctx, Opts, auto_answer, isolated, ms, open_item, research, text_of, vm_up, wait_for, wait_ready, write_module};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const VM: Opts = Opts { env: &[], vm: true, keychain: false };

// ================================================================ vm_smoke: §17 the app VM bridge

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn vm_smoke() {
    isolated("vm_smoke", VM, vm_smoke_case);
}

const VM_SERVER_TS: &str = r#"import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { statSync } from "node:fs";
import { release } from "node:os";

const server = new McpServer({ name: "hello", version: "0.1.0" });
server.registerTool("greet", { description: "Say hello", inputSchema: { name: z.string().optional() } },
  async ({ name }) => {
    const text = `Hello ${name ?? "there"} from ${process.platform} ${release()}`;
    return { content: [{ type: "text", text }], structuredContent: { openapp: { notify: text } } };
  });
server.registerTool("transcribe", { description: "Fake transcription", inputSchema: { path: z.string(), mime: z.string().optional() } }, async ({ path, mime }) => {
  const text = `heard ${statSync(path).size} bytes (${mime}) at ${path}`;
  return { content: [{ type: "text", text }], structuredContent: { openapp: { composer_insert: text } } };
});
server.registerTool("ask", { description: "Ask the Mac's on-device model through the host bridge", inputSchema: { prompt: z.string() } }, async ({ prompt }) => {
  const r = await fetch(`${process.env.OPENAPP_HOST_URL}/ai.generate`, {
    method: "POST",
    headers: { Authorization: `Bearer ${process.env.OPENAPP_HOST_TOKEN}`, "Content-Type": "application/json" },
    body: JSON.stringify({ prompt }),
  });
  const j = await r.json();
  const text = `${r.status} ${j.text ?? j.error}`;
  return { content: [{ type: "text", text }], structuredContent: { openapp: { notify: text } } };
});
await server.connect(new StdioServerTransport());
"#;

fn egress_for(ws: &std::path::Path, host: &str) -> Vec<Value> {
    research(ws).into_iter().filter(|r| r["kind"] == "egress" && r["data"]["host"] == host).map(|r| r["data"].clone()).collect()
}

/// Egress entries are written when a connection closes; wait briefly for one that matches.
async fn wait_egress(ws: &std::path::Path, host: &str, f: impl Fn(&Value) -> bool) -> bool {
    support::poll(Duration::from_secs(3), || egress_for(ws, host).iter().any(&f).then_some(())).await.is_some()
}

async fn vm_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    println!("workspace: {}  vm home: {}", ws.display(), openapp_lib::vm::vm_home().display());
    let sink = Arc::new(support::Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, Some("codex"), sink.clone())?;

    // auto-answer permission cards by host
    let rules: Vec<(&str, &str)> = vec![("example.com", "deny"), ("example.org", "deny"), ("api.github.com", "allow_workspace"), ("gitlab.com", "allow_once")];
    auto_answer(&k, &sink, move |c| {
        let title = c["title"].as_str().unwrap_or("");
        rules.iter().find(|(h, _)| title.contains(&format!("reach {h}"))).map(|(_, o)| o.to_string())
    });

    // 1) boot + Codex install + app-server initialize over the bridge
    let t = Instant::now();
    k.start(true).await?;
    let vm = k.vm().expect("app VM running after start");
    println!("VM {} timings: {}", vm.name, vm.timings.lock().unwrap());
    wait_ready(&k, Duration::from_secs(600)).await;
    let install = research(&k.state_dir).into_iter().find(|r| r["kind"] == "harness_install").map(|r| r["data"].clone());
    println!(
        "TIMING start→codex ready: {} ms (codex install: {})",
        ms(t),
        install.map(|i| format!("{} ms, first run", i["ms"])).unwrap_or_else(|| "cached on the volume".into())
    );
    let cfg = research(&k.state_dir).into_iter().find(|r| r["kind"] == "harness_config").map(|r| r["data"].clone()).unwrap_or_default();
    check!(cfg["runtime"] == "vm" && cfg["sandbox"] == "danger-full-access" && cfg["approval"] == "never", "codex app-server initialized in the VM, bypass mode ({cfg})");

    let d = k.describe();
    check!(d["capabilities"]["vm"] == true && d["capabilities"]["egress"] == "proxy+consent" && d["vm"]["workspace"] == "/workspace" && d["host"]["tools"]["node"].is_string(), "describe: capabilities.vm, egress proxy+consent, guest tools {}", d["host"]["tools"]);

    // 2) account/read: not signed in; the picker card says so
    let c = k.codex.lock().unwrap().clone().expect("codex conn");
    let t = Instant::now();
    let acct = c.rpc.request("account/read", json!({ "refreshToken": false }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
    println!("account/read ({} ms): {acct}", ms(t));
    let card = k.snapshot().harnesses.into_iter().find(|h| h.id == "codex").unwrap();
    check!(acct["account"].is_null() && card.signed_in == Some(false) && card.login.as_ref().is_some_and(|l| l["state"] == "required"), "VM Codex not signed in; picker card: {:?} / {:?}", card.note, card.login);

    // 3) kernel MCP from the guest through the forwarder
    let ep = k.mcp.get().unwrap();
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}"#;
    let t = Instant::now();
    let (code, out, err) = vm
        .sh(
            &format!("curl -sS -m 10 -X POST http://127.0.0.1:{}/mcp -H \"Authorization: Bearer $T\" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d '{init}'", openapp_lib::vm::MCP_PORT),
            &[("T".into(), ep.token.clone())],
        )
        .await
        .map_err(anyhow::Error::msg)?;
    check!(code == 0 && out.contains("\"openapp\""), "kernel MCP reachable from the guest at 127.0.0.1:{} ({} ms){}", openapp_lib::vm::MCP_PORT, ms(t), if code != 0 { format!(": {err}") } else { String::new() });
    let st = c.rpc.request("mcpServerStatus/list", json!({}), Duration::from_secs(30)).await;
    println!("codex mcpServerStatus/list: {}", st.map(|v| v.to_string()).unwrap_or_else(|e| e).chars().take(300).collect::<String>());

    // 4) egress proxy
    let agent = openapp_lib::vm::proxy_env("agent");
    let t = Instant::now();
    let (code, out, err) = vm.sh("npm view left-pad version", &agent).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out.trim() == "1.3.0", "npm registry allowed by default ({} ms){}", ms(t), if code != 0 { format!(": {err}") } else { String::new() });
    check!(wait_egress(&k.state_dir, "registry.npmjs.org", |e| e["decision"] == "allow" && e["reason"] == "default" && e["actor"] == "agent").await, "egress log: registry.npmjs.org allow/default/agent");

    let curl = |url: &str| format!("curl -sS -m 30 -o /dev/null -w '%{{http_code}}' '{url}'");
    let t = Instant::now();
    let (code, out, err) = vm.sh(&curl("https://example.com/"), &agent).await.map_err(anyhow::Error::msg)?;
    let shown = sink.card_titles().iter().any(|(_, t)| t.contains("reach example.com"));
    check!(shown && code != 0, "example.com: permission card shown, denied, curl failed (exit {code}, {} ms: {})", ms(t), err.trim().lines().last().unwrap_or(out.trim()));
    check!(wait_egress(&k.state_dir, "example.com", |e| e["decision"] == "deny" && e["actor"] == "agent").await, "egress log: example.com deny");

    let t = Instant::now();
    let (code, out, err) = vm.sh(&curl("https://gitlab.com/"), &agent).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && (out.starts_with('2') || out.starts_with('3')), "gitlab.com: allowed once → HTTP {} ({} ms){}", out.trim(), ms(t), if code != 0 { format!(": {err}") } else { String::new() });
    let (code2, _, _) = vm.sh(&curl("https://gitlab.com/"), &agent).await.map_err(anyhow::Error::msg)?;
    let gitlab_cards = sink.card_titles().iter().filter(|(_, t)| t.contains("reach gitlab.com")).count();
    check!(code2 == 0 && gitlab_cards == 1, "gitlab.com second connection within the once window: no new card");
    // §21 G3: GitHub release downloads are allowed by default (still logged)
    let (code, out, _) = vm.sh(&curl("https://github.com/"), &agent).await.map_err(anyhow::Error::msg)?;
    let carded = sink.card_titles().iter().any(|(_, t)| t.contains("reach github.com"));
    check!(code == 0 && !carded && (out.starts_with('2') || out.starts_with('3')), "github.com: default allow (no card) → HTTP {}", out.trim());

    let (code, out, _) = vm.sh(&curl("https://api.github.com/zen"), &agent).await.map_err(anyhow::Error::msg)?;
    let g = k.snapshot().grants;
    check!(code == 0 && out.starts_with('2') && g.iter().any(|x| x["id"] == "network:api.github.com" && x["allowed"] == true), "api.github.com: allowed for this workspace → grants.json + host data grants");
    k.host_action("grant.revoke", &[&json!({ "id": "network:api.github.com" })]).map_err(anyhow::Error::msg)?;
    check!(!k.snapshot().grants.iter().any(|x| x["id"] == "network:api.github.com"), "grant.revoke removed the grant");

    // the agent cannot grant itself network access: kernel state is outside /workspace
    let (wc, _, _) = vm
        .sh(r#"mkdir -p /workspace/.openapp && echo '{"network":{"example.org":{"allowed":true,"scope":"workspace"}}}' > /workspace/.openapp/grants.json"#, &[])
        .await
        .map_err(anyhow::Error::msg)?;
    let (code, _, _) = vm.sh(&curl("https://example.org/"), &agent).await.map_err(anyhow::Error::msg)?;
    let carded = sink.card_titles().iter().any(|(_, t)| t.contains("reach example.org"));
    check!(wc == 0 && code != 0 && carded && !k.state_dir.starts_with(&ws), "guest write to /workspace/.openapp/grants.json has no effect: example.org still asked and denied (state in {})", k.state_dir.display());
    let (code, _, _) = vm.sh("curl -sS -m 8 --noproxy '*' -o /dev/null https://registry.npmjs.org/", &[]).await.map_err(anyhow::Error::msg)?;
    check!(code != 0, "no direct network from the guest (curl without proxy exit {code})");
    let (code, out, _) = vm
        .sh(&format!("curl -sS -m 8 --noproxy '' -o /dev/null -w '%{{http_code}}' -x http://agent@127.0.0.1:{} http://127.0.0.1:{}/mcp", openapp_lib::vm::PROXY_PORT, ep.port), &[])
        .await
        .map_err(anyhow::Error::msg)?;
    check!(out.trim() == "403", "proxy refuses the laptop's own services (127.0.0.1:{} → HTTP {} exit {code})", ep.port, out.trim());

    // 5) module server in the VM (node_modules in /workspace, installed in the guest through the proxy)
    if !ws.join("node_modules/@modelcontextprotocol/sdk").exists() {
        std::fs::write(ws.join("package.json"), r#"{"name":"vm-smoke-ws","private":true,"type":"module"}"#)?;
        let t = Instant::now();
        let (code, _, err) = vm
            .exec_out("npm", &["i".into(), "--silent".into(), "--no-audit".into(), "--no-fund".into(), "@modelcontextprotocol/sdk".into(), "zod".into()], &openapp_lib::vm::proxy_env("agent"), Some("/workspace"))
            .await
            .map_err(anyhow::Error::msg)?;
        check!(code == 0, "npm install in the guest ({} ms){}", ms(t), if code != 0 { format!(": {err}") } else { String::new() });
    }
    let m = ws.join("modules/hello");
    std::fs::create_dir_all(m.join("views"))?;
    std::fs::write(m.join("server.ts"), VM_SERVER_TS)?;
    std::fs::write(
        m.join("openapp.json"),
        r#"{ "id": "hello", "name": "Hello", "server": { "command": ["node", "server.ts"] },
            "views": { "button": { "kind": "spec", "file": "views/button.json" } },
            "placements": [ { "view": "button", "anchor": "composer.actions" } ] }"#,
    )?;
    std::fs::write(
        m.join("views/button.json"),
        r#"{ "type": "stack", "direction": "row", "children": [
              { "type": "icon-button", "icon": "sparkles", "label": "Greet", "action": { "tool": "greet", "args": { "name": "vm" } } },
              { "type": "icon-button", "icon": "mic", "label": "Dictate", "action": { "host": "audio.record", "tool": "transcribe", "arg": "path" } } ] }"#,
    )?;
    // host writes reach the guest after up to ~5 s (virtio-fs attr cache)
    let t = Instant::now();
    let mut loaded = Err(json!(null));
    for _ in 0..40 {
        loaded = k.module_load("hello").await;
        if loaded.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let lv = loaded.unwrap_or_else(|e| e);
    check!(lv["ok"] == true && lv["runs_in"].as_str().is_some_and(|s| s.contains("VM")), "module hello loaded in the VM ({} ms incl. mount lag): {}", ms(t), lv["runs_in"]);
    let placement = k.snapshot().layout.placements.iter().find(|p| p.module == "hello").map(|p| p.id.clone()).expect("default placement");
    let t = Instant::now();
    let r = k.ui_action(&placement, json!({ "tool": "greet", "args": { "name": "vm" } }), None).await;
    let note = sink.all("oa://notify").last().cloned().unwrap_or_default();
    check!(r.is_ok() && note["text"].as_str().is_some_and(|t| t.contains("from linux")), "ui_action → module tool in the VM ({} ms): {}", ms(t), note["text"]);

    // 5b) module server in the VM → host services over the bridge (OPENAPP_HOST_URL)
    let t = Instant::now();
    let r = k.ui_action(&placement, json!({ "tool": "ask", "args": { "prompt": "Reply with exactly: bridge ok" } }), None).await;
    let note = sink.all("oa://notify").last().cloned().unwrap_or_default();
    check!(r.is_ok() && note["text"].as_str().is_some_and(|t| t.starts_with("200") && t.to_lowercase().contains("bridge ok")), "module in the VM → POST $OPENAPP_HOST_URL/ai.generate ({} ms): {}", ms(t), note["text"]);

    // 6) save_audio: written through the bridge, visible to the guest module at once
    let mut g: Value = std::fs::read(k.state_dir.join("grants.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
    g["microphone"] = json!({ "hello": true });
    std::fs::write(k.state_dir.join("grants.json"), serde_json::to_vec_pretty(&g)?)?;
    let bytes: Vec<u8> = (0..4321u32).map(|i| (i * 7 % 251) as u8).collect();
    let b64 = {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for ch in bytes.chunks(3) {
            let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                s.push(if i <= ch.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    };
    let t = Instant::now();
    let r = openapp_lib::primitives::audio::save(&k, &placement, json!(b64), "audio/webm", Some(json!({ "host": "audio.record", "tool": "transcribe", "arg": "path" })), Some(1000)).await;
    let ins = sink.all("oa://composer").last().cloned().unwrap_or_default();
    let text = ins["insert"].as_str().unwrap_or("").to_string();
    check!(r.is_ok() && text.starts_with("heard 4321 bytes") && text.contains("/workspace/modules/hello/data/recordings/"), "save_audio visible in the guest at once ({} ms): {text}{}", ms(t), r.err().map(|e| format!(" ERR {e}")).unwrap_or_default());
    let host_file = text.split(" at ").nth(1).map(|p| ws.join(p.trim_start_matches("/workspace/")));
    check!(host_file.as_ref().is_some_and(|p| std::fs::metadata(p).map(|m| m.len()) .ok() == Some(4321)), "the recording is also on the host");

    // 7) login: callback forward host → guest (no browser opened, then cancelled)
    let t = Instant::now();
    let l = openapp_lib::codex::login(&k, false, false).await.map_err(anyhow::Error::msg)?;
    let port = l["port"].as_u64().unwrap_or(1455) as u16;
    let resp = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.ok()?;
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await;
        Some(String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_string())
    }
    .await
    .unwrap_or_default();
    let card = k.snapshot().harnesses.into_iter().find(|h| h.id == "codex").unwrap();
    check!(l["method"] == "browser" && resp.starts_with("HTTP/1.1") && card.login.as_ref().is_some_and(|x| x["state"] == "browser"), "login: auth URL ready, host 127.0.0.1:{port} → guest callback server answers ({resp:?}, {} ms)", ms(t));
    openapp_lib::codex::login_cancel(&k).await;

    // 8) optional: Codex's own traffic goes through the proxy
    if std::env::var("OPENAPP_SMOKE_CODEX_PROXY").is_ok_and(|v| v == "1") {
        codex_proxy_probe(&k, &k.state_dir, &c).await?;
    }

    let egress = research(&k.state_dir).into_iter().filter(|r| r["kind"] == "egress").count();
    println!("research log: {egress} egress entries");
    let t = Instant::now();
    k.shutdown();
    println!("TIMING shutdown (VM stop): {} ms; total {} s", ms(t), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

/// Fake API key (not a credential) → one turn → which hosts Codex reached through the proxy.
async fn codex_proxy_probe(k: &Arc<Kernel>, ws: &std::path::Path, c: &Arc<openapp_lib::codex::CodexConn>) -> anyhow::Result<()> {
    let r = c.rpc.request("account/login/start", json!({ "type": "apiKey", "apiKey": "sk-openapp-smoke-not-a-real-key" }), Duration::from_secs(30)).await;
    println!("probe: apiKey login → {}", r.map(|v| v.to_string()).unwrap_or_else(|e| e));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sid = k.active_session();
    let t = Instant::now();
    openapp_lib::harness::run_turn(k.clone(), sid, "Say hi.".into(), false).await;
    println!("probe: turn finished in {} ms", t.elapsed().as_millis());
    let hosts: Vec<String> = research(ws)
        .into_iter()
        .filter(|r| r["kind"] == "egress" && r["data"]["actor"] == "agent" && r["data"]["host"].as_str().is_some_and(|h| h.contains("openai") || h.contains("chatgpt")))
        .map(|r| format!("{}:{} {} {}B↓", r["data"]["host"].as_str().unwrap_or(""), r["data"]["port"], r["data"]["method"].as_str().unwrap_or(""), r["data"]["bytes_down"]))
        .collect();
    println!("probe: Codex egress via proxy: {hosts:?}");
    let errs: Vec<String> = k.snapshot().chat.iter().filter(|i| i["kind"] == "error").map(|i| i["text"].as_str().unwrap_or("").chars().take(200).collect()).collect();
    println!("probe: chat errors: {errs:?}");
    let _ = c.rpc.request("account/logout", Value::Null, Duration::from_secs(30)).await;
    let a = c.rpc.request("account/read", json!({}), Duration::from_secs(30)).await.unwrap_or_default();
    check!(!hosts.is_empty(), "Codex model traffic goes through the proxy (signed out again: account {})", a["account"]);
    Ok(())
}

// ================================================================ services_smoke: §21 G2/G3 services in the VM

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn services_smoke() {
    isolated("services_smoke", VM, services_smoke_case);
}

fn svc(k: &Kernel, module: &str, name: &str) -> Value {
    prims::services::statuses(k, Some(module)).into_iter().find(|s| s["name"] == name).unwrap_or(Value::Null)
}

async fn action(k: &Arc<Kernel>, name: &str, args: Value) -> Result<Value, String> {
    prims::call_action(k, name, args, Caller::new("agent")).await.unwrap_or_else(|| Err(format!("no action {name}")))
}


async fn oasvc_get(k: &Arc<Kernel>, url: &str) -> (u16, String) {
    let req = tauri::http::Request::builder().method("GET").uri(url).body(Vec::new()).unwrap();
    let r = prims::ports::handle(k, req).await;
    (r.status().as_u16(), String::from_utf8_lossy(r.body()).to_string())
}

/// Read a PTY topic until `pat` appears (or timeout); returns the text so far.
async fn pty_read(rx: &mut channels::Receiver, acc: &mut String, pat: &str, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while !acc.contains(pat) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(m)) => match &m.data {
                ChannelData::Binary(b) => acc.push_str(&String::from_utf8_lossy(b)),
                ChannelData::Json(v) => acc.push_str(&format!("\n[json {v}]\n")),
            },
            _ => return acc.contains(pat),
        }
    }
    true
}

async fn services_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    println!("workspace: {}  vm home: {}", ws.display(), openapp_lib::vm::vm_home().display());
    let lan_allow = std::env::var("OPENAPP_SMOKE_LAN").is_ok_and(|v| v == "1");
    let lan_port: u16 = std::env::var("OPENAPP_SMOKE_LAN_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(18093);

    // modules: web (apt + http.server + LAN), term (pty only), gui (display)
    let m = ws.join("modules/web");
    std::fs::create_dir_all(m.join("site"))?;
    std::fs::write(m.join("site/index.html"), "<h1>hello from the VM</h1>")?;
    std::fs::write(
        m.join("openapp.json"),
        serde_json::to_vec_pretty(&json!({
            "id": "web", "name": "Web smoke",
            "apt": ["tree"],
            "services": [
                { "name": "http", "command": ["python3", "-m", "http.server", "8080", "--bind", "127.0.0.1"], "cwd": "site",
                  "restart": "always", "health": { "http": "http://127.0.0.1:8080/" }, "ports": [{ "port": 8080, "expose": "view" }] },
                { "name": "lan", "command": ["python3", "-m", "http.server", lan_port.to_string(), "--bind", "127.0.0.1"], "cwd": "site",
                  "health": { "tcp": lan_port }, "ports": [{ "port": lan_port, "expose": "lan" }] }
            ]
        }))?,
    )?;
    let g = ws.join("modules/gui");
    std::fs::create_dir_all(&g)?;
    std::fs::write(g.join("openapp.json"), serde_json::to_vec_pretty(&json!({ "id": "gui", "name": "GUI smoke", "display": { "command": ["xterm"], "width": 1024, "height": 640 } }))?)?;
    let t_mod = ws.join("modules/term");
    std::fs::create_dir_all(&t_mod)?;
    std::fs::write(t_mod.join("openapp.json"), r#"{ "id": "term", "name": "Terminal smoke" }"#)?;

    let sink = Arc::new(support::Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    // auto-answer the LAN consent card
    let lan_opt = if lan_allow { "allow_workspace" } else { "deny" };
    auto_answer(&k, &sink, move |c| Some(if c["title"].as_str().unwrap_or("").contains("local network") { lan_opt } else { "deny" }.into()));

    // 1) boot the Debian guest
    let t = Instant::now();
    k.start(false).await?;
    let vm = k.ensure_vm().await.map_err(anyhow::Error::msg)?;
    println!("VM {} timings: {}", vm.name, vm.timings.lock().unwrap());
    println!("TIMING start→VM ready: {} ms", ms(t));
    let (_, os, _) = vm.sh(". /etc/os-release; echo $PRETTY_NAME; ldd --version | head -1; node --version; df -h /opt/openapp | tail -1; cat /etc/openapp-guest | tr '\\n' ' '", &[]).await.map_err(anyhow::Error::msg)?;
    println!("guest: {}", os.trim().replace('\n', " | "));
    check!(os.contains("Debian") && os.contains("GLIBC") && os.contains("v24."), "Debian glibc guest with node 24");
    let tools = vm.guest_tools.lock().unwrap().clone();
    check!(["apt-get", "gcc", "Xvfb", "x11vnc", "websockify", "qemu-x86_64", "bwrap", "rg"].iter().all(|t| tools[*t].is_string()), "guest tools: {tools}");

    // web/term/gui load (no MCP servers) → services start
    for id in ["web", "term", "gui"] {
        let r = k.module_load(id).await;
        check!(r.is_ok(), "module {id} loaded: {}", r.map(|v| v["status"].clone()).unwrap_or_else(|e| e));
    }

    // 2) apt through the proxy
    let t = Instant::now();
    let st = prims::services::state(&k);
    let apt_done = wait_for(300, || st.mods.lock().unwrap().get("web").is_some_and(|m| m.apt == "ok" || m.apt.starts_with("failed"))).await;
    let apt = st.mods.lock().unwrap().get("web").map(|m| m.apt.clone()).unwrap_or_default();
    let (code, out, _) = vm.sh("tree --version", &[]).await.map_err(anyhow::Error::msg)?;
    check!(apt_done && apt == "ok" && code == 0, "apt install tree through the proxy ({} ms): {apt} / {}", ms(t), out.trim());
    let debian = research(&k.state_dir).into_iter().any(|r| r["kind"] == "egress" && r["data"]["host"] == "deb.debian.org" && r["data"]["actor"] == "apt" && r["data"]["decision"] == "allow");
    check!(debian, "egress log: deb.debian.org allow (actor apt)");

    // 3) supervised http.server with health check
    let t = Instant::now();
    check!(wait_for(60, || svc(&k, "web", "http")["healthy"] == true).await, "service web/http healthy ({} ms): {}", ms(t), svc(&k, "web", "http"));
    let hd = prims::host_data(&k);
    check!(hd["services"].as_array().is_some_and(|a| a.iter().any(|s| s["id"] == "web/http")), "host data services: {}", hd["services"]);
    let d = k.describe();
    check!(d["services"]["running"].as_array().is_some_and(|a| !a.is_empty()) && d["primitives"].as_array().is_some_and(|a| a.iter().any(|p| p["name"] == "services")), "describe: services + primitives entry");
    let (code, _, _) = vm.sh("pkill -f '[h]ttp.server 8080'", &[]).await.map_err(anyhow::Error::msg)?;
    let t = Instant::now();
    let restarted = wait_for(30, || svc(&k, "web", "http")["restarts"].as_u64().unwrap_or(0) >= 1).await;
    let healthy = wait_for(30, || svc(&k, "web", "http")["healthy"] == true && svc(&k, "web", "http")["state"] == "running").await;
    check!(code == 0 && restarted && healthy, "killed http.server → restarted and healthy again ({} ms): restarts {}", ms(t), svc(&k, "web", "http")["restarts"]);
    let since = svc(&k, "web", "http")["since"].clone();
    let r = k.module_load("web").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    check!(r.is_ok() && svc(&k, "web", "http")["since"] == since, "module reload with the same declaration keeps the service running");
    let t = Instant::now();
    action(&k, "service.restart", json!({ "module": "web", "name": "http" })).await.map_err(anyhow::Error::msg)?;
    let again = wait_for(30, || svc(&k, "web", "http")["restarts"].as_u64().unwrap_or(0) >= 2 && svc(&k, "web", "http")["healthy"] == true).await;
    let (_, n, _) = vm.sh("pgrep -fc '^python3 -m http.server 8080'", &[]).await.map_err(anyhow::Error::msg)?;
    check!(again && n.trim() == "1", "service.restart (process group TERM) → running and healthy again, one process ({} ms): {} / {}", ms(t), n.trim(), svc(&k, "web", "http"));
    let log = std::fs::read_to_string(ws.join(".openapp/logs/web.http.log")).unwrap_or_default();
    check!(log.matches("=== ").count() >= 3 && log.contains("GET / HTTP"), "log .openapp/logs/web.http.log has starts, exit and requests ({} lines)", log.lines().count());

    // 4) oasvc:// from the host (protocol handler, no window) + gateway
    let t = Instant::now();
    let (status, body) = oasvc_get(&k, "oasvc://web/8080/index.html").await;
    check!(status == 200 && body.contains("hello from the VM"), "oasvc://web/8080/index.html → {status} ({} ms)", ms(t));
    let (status, body) = oasvc_get(&k, "oasvc://web/8080/").await;
    check!(status == 200 && body.contains("hello from the VM"), "oasvc://web/8080/ → index {status}");
    let (status, _) = oasvc_get(&k, "oasvc://web/9999/").await;
    check!(status == 404, "oasvc:// to an undeclared port is refused ({status})");
    let u = action(&k, "service.url", json!({ "module": "web", "port": 8080, "path": "/index.html" })).await.map_err(anyhow::Error::msg)?;
    let r = reqwest::get(u["http"].as_str().unwrap()).await?;
    let (s, b) = (r.status().as_u16(), r.text().await?);
    check!(s == 200 && b.contains("hello from the VM"), "gateway {} → {s}", u["http"]);
    let bad = reqwest::get(u["http"].as_str().unwrap().replace("/g", "/gx")).await?.status().as_u16();
    check!(bad == 403, "gateway with a wrong token → {bad}");
    let other = prims::call_action(&k, "service.url", json!({ "module": "web", "port": 8080 }), Caller::new("view:gui")).await.unwrap();
    check!(other.is_err(), "a view of another module cannot get web's URL: {}", other.err().unwrap_or_default());

    // 5) PTY over channels
    let t = Instant::now();
    let p = action(&k, "pty.open", json!({ "module": "term", "command": "echo hi; tput cols; tput lines; sleep 0.3", "cols": 100, "rows": 30 })).await.map_err(anyhow::Error::msg)?;
    let mut rx = channels::subscribe(&k, p["topic"].as_str().unwrap(), true);
    let mut acc = String::new();
    let got = pty_read(&mut rx, &mut acc, "\"exit\"", 20).await;
    check!(got && acc.contains("hi") && acc.contains("100") && acc.contains("30") && acc.contains("\"code\":0"), "pty.open echo hi; tput cols/lines → over channel {} ({} ms): {:?}", p["topic"], ms(t), acc.replace("\r", ""));
    let p = action(&k, "pty.open", json!({ "module": "term", "cols": 80, "rows": 24 })).await.map_err(anyhow::Error::msg)?;
    let mut rx = channels::subscribe(&k, p["topic"].as_str().unwrap(), true);
    let id = p["id"].as_str().unwrap().to_string();
    action(&k, "pty.resize", json!({ "id": id, "cols": 132, "rows": 40 })).await.map_err(anyhow::Error::msg)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    channels::publish(&k, p["input"].as_str().unwrap(), ChannelData::Json(json!("echo COLS=$(tput cols)x$(tput lines)\n")));
    let mut acc = String::new();
    let got = pty_read(&mut rx, &mut acc, "COLS=132x40", 15).await;
    check!(got, "interactive bash: input over {} + pty.resize → COLS=132x40 ({:?})", p["input"], acc.chars().rev().take(400).collect::<String>().chars().rev().collect::<String>());
    channels::publish(&k, p["input"].as_str().unwrap(), ChannelData::Json(json!({ "resize": { "cols": 90, "rows": 20 } })));
    tokio::time::sleep(Duration::from_millis(300)).await;
    action(&k, "pty.write", json!({ "id": id, "data": "echo NOW=$(tput cols); exit\n" })).await.map_err(anyhow::Error::msg)?;
    let got = pty_read(&mut rx, &mut acc, "\"exit\"", 15).await;
    check!(got && acc.contains("NOW=90"), "resize message on the input topic + pty.write + exit → NOW=90, exit event");

    // 6) display: Xvfb + xterm + x11vnc + websockify
    let t = Instant::now();
    let ok = wait_for(60, || svc(&k, "gui", "display")["healthy"] == true).await;
    check!(ok, "display service healthy ({} ms): {}", ms(t), svc(&k, "gui", "display"));
    let (_, ps, _) = vm.sh("ps -eo comm | sort | uniq | tr '\\n' ' '", &[]).await.map_err(anyhow::Error::msg)?;
    check!(ps.contains("Xvfb") && ps.contains("xterm") && ps.contains("x11vnc"), "display processes: {}", ps.trim());
    let du = action(&k, "display.url", json!({ "module": "gui" })).await.map_err(anyhow::Error::msg)?;
    let (status, body) = oasvc_get(&k, du["oasvc"].as_str().unwrap()).await;
    check!(status == 200 && body.contains("noVNC"), "noVNC page via {} → {status}", du["oasvc"]);
    let page = reqwest::get(du["page"].as_str().unwrap()).await?.status().as_u16();
    check!(page == 200, "noVNC page via the gateway → {page}");
    // WebSocket handshake + RFB banner through the gateway
    let ws_url = url::Url::parse(du["ws"].as_str().unwrap())?;
    let mut s = tokio::net::TcpStream::connect((ws_url.host_str().unwrap(), ws_url.port().unwrap())).await?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n",
        ws_url.path()
    );
    s.write_all(req.as_bytes()).await?;
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && !String::from_utf8_lossy(&got).contains("RFB 00") {
        match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&got);
    check!(text.starts_with("HTTP/1.1 101") && text.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=") && text.contains("RFB 003.008"), "VNC WebSocket through the gateway: 101 Switching Protocols + RFB banner ({:?})", text.lines().next().unwrap_or(""));
    drop(s);

    // 7) qemu-user: an amd64 busybox
    let t = Instant::now();
    let script = r#"set -e; cd /tmp; f=$(apt-cache show busybox-static | awk '/^Filename:/{print $2; exit}' | sed 's/_arm64\.deb$/_amd64.deb/');
curl -fsS -o bb.deb "https://deb.debian.org/debian/$f"; rm -rf bb; dpkg-deb -x bb.deb bb; file -b bb/usr/bin/busybox | cut -d, -f1-2;
q=$(command -v qemu-x86_64-static || command -v qemu-x86_64); "$q" bb/usr/bin/busybox echo hello-from-amd64;
if [ -e /proc/sys/fs/binfmt_misc/qemu-x86_64 ]; then echo binfmt=yes; bb/usr/bin/busybox echo direct-ok; else echo binfmt=no; fi"#;
    let (code, out, err) = vm.sh(script, &openapp_lib::vm::proxy_env("agent")).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out.contains("x86-64") && out.contains("hello-from-amd64"), "qemu-user amd64 busybox ({} ms): {} {}", ms(t), out.trim().replace('\n', " | "), err.trim());

    // 8) expose "lan": consent card → host listener on 0.0.0.0:<port> (or denied)
    let t = Instant::now();
    if lan_allow {
        let on = wait_for(60, || svc(&k, "web", "lan")["ports"][0]["lan"].as_str().is_some_and(|s| s.starts_with("on:"))).await;
        let r = reqwest::get(format!("http://127.0.0.1:{lan_port}/index.html")).await;
        let body = match r {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(e) => e.to_string(),
        };
        let granted = k.snapshot().grants.iter().any(|g| g["id"] == format!("lan:web:{lan_port}") && g["allowed"] == true);
        check!(on && granted && body.contains("hello from the VM"), "expose lan: card allowed → {} ({} ms), grant lan:web:{lan_port}", svc(&k, "web", "lan")["ports"][0]["lan"], ms(t));
    } else {
        let denied = wait_for(60, || svc(&k, "web", "lan")["ports"][0]["lan"] == "denied").await;
        let closed = tokio::net::TcpStream::connect(("127.0.0.1", lan_port)).await.is_err();
        let carded = sink.card_titles().iter().any(|(_, t)| t.contains("on your local network"));
        let g = k.snapshot().grants.iter().any(|g| g["id"] == format!("lan:web:{lan_port}") && g["allowed"] == false);
        check!(denied && closed && carded && g, "expose lan: consent card shown, denied → no host listener, grant lan:web:{lan_port}=false (OPENAPP_SMOKE_LAN=1 tests the listener)");
    }

    // 9) unload stops services
    let t = Instant::now();
    k.module_unload("web", false).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let gone = wait_for(20, || prims::services::statuses(&k, Some("web")).is_empty()).await;
    let (code, _, _) = vm.sh("pgrep -f '[h]ttp.server'", &[]).await.map_err(anyhow::Error::msg)?;
    let lan_closed = !lan_allow || tokio::net::TcpStream::connect(("127.0.0.1", lan_port)).await.is_err();
    check!(gone && code != 0 && lan_closed, "unload web: services stopped, guest processes gone, LAN listener closed ({} ms)", ms(t));
    k.module_unload("gui", false).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let gone = wait_for(20, || prims::services::statuses(&k, Some("gui")).is_empty()).await;
    let (code, _, _) = vm.sh("pgrep -x Xvfb || pgrep -x xterm || pgrep -x x11vnc", &[]).await.map_err(anyhow::Error::msg)?;
    check!(gone, "unload gui: display service gone from status");
    check!(code != 0, "unload gui: Xvfb process group stopped");

    let t = Instant::now();
    k.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("TIMING shutdown: {} ms; total {} s", ms(t), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ pi_smoke: §11 Pi in the VM

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn pi_smoke() {
    isolated("pi_smoke", VM, pi_smoke_case);
}

/// OpenAI-compatible fake provider for section 9 (runs in the guest; logs every request body to /tmp/fake-llm.log).
const FAKE_LLM: &str = r#"import http from "node:http";
import fs from "node:fs";
http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    if (req.method !== "POST") { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ data: [] })); return; }
    fs.appendFileSync("/tmp/fake-llm.log", body.replace(/\n/g, " ") + "\n");
    let j = {};
    try { j = JSON.parse(body); } catch {}
    const hasTool = (j.messages || []).some((m) => m.role === "tool");
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    const base = { id: "c1", object: "chat.completion.chunk", created: 0, model: "fake-tools" };
    const send = (o) => res.write("data: " + JSON.stringify({ ...base, ...o }) + "\n\n");
    if (!hasTool) {
      const calls = [
        { index: 0, id: "call_find", type: "function", function: { name: "find", arguments: JSON.stringify({ pattern: "*.md", path: "/workspace/smoke-find" }) } },
        { index: 1, id: "call_read", type: "function", function: { name: "read", arguments: JSON.stringify({ path: "/workspace/smoke-find/missing.txt" }) } },
      ];
      send({ choices: [{ index: 0, delta: { role: "assistant", content: null, tool_calls: calls }, finish_reason: null }] });
      send({ choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] });
    } else {
      send({ choices: [{ index: 0, delta: { role: "assistant", content: "done" }, finish_reason: null }] });
      send({ choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
    }
    send({ choices: [], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } });
    res.end("data: [DONE]\n\n");
  });
}).listen(9911, "127.0.0.1");
"#;

fn pi(k: &Kernel) -> Arc<openapp_lib::pi::PiConn> {
    k.pi.lock().unwrap().clone().expect("pi conn")
}

/// Wait for a Pi `notify` that starts with `prefix`; returns the rest.
async fn notice(c: &openapp_lib::pi::PiConn, prefix: &str, skip: usize, limit: Duration) -> Option<String> {
    support::poll(limit, || c.notices().into_iter().skip(skip).find(|n| n.starts_with(prefix)).map(|n| n[prefix.len()..].trim().to_string())).await
}

/// A dialog command answers its `prompt` only after the dialog closes: send it without waiting.
fn send_cmd(c: &Arc<openapp_lib::pi::PiConn>, message: &str) {
    let (c, m) = (c.clone(), message.to_string());
    tokio::spawn(async move {
        if let Err(e) = c.request(json!({ "type": "prompt", "message": m }), Duration::from_secs(60)).await {
            println!("  ({m}: {e})");
        }
    });
}


async fn pi_smoke_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    assert_ne!(openapp_lib::vm::vm_home(), dirs::home_dir().unwrap().join(".openapp/vm"), "never the app's VM state");
    let t_all = Instant::now();
    println!("workspace: {}  vm home: {}", ws.display(), openapp_lib::vm::vm_home().display());
    let sink = Arc::new(support::Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, Some("pi"), sink.clone())?;

    // 1) boot + Pi install + rpc get_state
    let t = Instant::now();
    k.start(true).await?;
    let vm = k.vm().expect("app VM running after start");
    wait_ready(&k, Duration::from_secs(900)).await;
    let install = research(&k.state_dir).into_iter().find(|r| r["kind"] == "harness_install").map(|r| r["data"].clone());
    println!(
        "TIMING start→pi ready: {} ms (pi install: {})",
        ms(t),
        install.map(|i| format!("{} ms, ok {}", i["ms"], i["ok"])).unwrap_or_else(|| "cached on the volume".into())
    );
    let cfg = research(&k.state_dir).into_iter().find(|r| r["kind"] == "harness_config").map(|r| r["data"].clone()).unwrap_or_default();
    check!(cfg["runtime"] == "vm" && cfg["version"] == openapp_lib::pi::PI_VERSION && cfg["mode"] == "rpc", "pi --mode rpc started in the VM ({cfg})");
    let c = pi(&k);
    let st = c.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
    check!(st["sessionId"].is_string() && st["isStreaming"] == false, "get_state: session {} streaming {} model {}", st["sessionId"], st["isStreaming"], st["model"]["id"]);
    let (code, ver, _) = vm.sh(&format!("PI_OFFLINE=1 PI_CODING_AGENT_DIR={} {} --version", openapp_lib::pi::VM_PI_HOME, openapp_lib::pi::bin()), &[]).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && ver.contains(openapp_lib::pi::PI_VERSION), "pinned Pi in the VM volume: {}", ver.trim());

    // 2) picker card: not signed in, sign-in choices
    let card = k.snapshot().harnesses.into_iter().find(|h| h.id == "pi").expect("pi card");
    let methods: Vec<String> = card.login.as_ref().and_then(|l| l["methods"].as_array().cloned()).unwrap_or_default().iter().filter_map(|m| m["id"].as_str().map(String::from)).collect();
    check!(card.vm && card.signed_in == Some(false) && methods == ["chatgpt", "claude", "apikey"], "picker card: {} / {:?} / methods {methods:?}", card.name, card.note);

    // 3) framing + tools as Pi sees them (MCP connects in the background)
    let mut info = Value::Null;
    for attempt in 0..20 {
        let skip = c.notices().len();
        c.request(json!({ "type": "prompt", "message": "/openapp-check" }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
        let n = notice(&c, "openapp-check info", skip, Duration::from_secs(10)).await.unwrap_or_default();
        info = serde_json::from_str(&n).unwrap_or(Value::Null);
        let has = info["tools"].as_array().is_some_and(|a| a.iter().any(|t| t == "mcp__openapp__openapp_describe"));
        if has {
            break;
        }
        if attempt == 19 {
            println!("tools: {}", info["tools"]);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let tools: Vec<String> = info["tools"].as_array().cloned().unwrap_or_default().iter().filter_map(|t| t.as_str().map(String::from)).collect();
    let mcp_tools = tools.iter().filter(|t| t.starts_with("mcp__openapp__")).count();
    check!(
        info["promptHead"].as_str().is_some_and(|h| h.starts_with("You are the builder inside OpenApp")),
        "SYSTEM.md is Pi's system prompt ({} chars): {:?}",
        info["promptChars"],
        info["promptHead"].as_str().unwrap_or("").chars().take(60).collect::<String>()
    );
    check!(
        mcp_tools >= 10 && tools.iter().any(|t| t == "ask_user") && info["active"].as_array().is_some_and(|a| a.iter().any(|t| t == "mcp__openapp__openapp_describe")),
        "Pi sees {mcp_tools} openapp MCP tools (active, direct) + ask_user; all tools: {}",
        tools.len()
    );
    let token = k.mcp.get().unwrap().token.clone();
    let mut env = openapp_lib::pi::guest_env(Some(&token));
    env.retain(|(k, _)| k.starts_with("PI_") || k == "OPENAPP_MCP_TOKEN" || k == "HOME" || k.to_lowercase().contains("proxy"));
    let t = Instant::now();
    let (code, out, err) = vm.exec_out(&openapp_lib::pi::bin(), &["mcp".into(), "list".into()], &env, Some("/workspace")).await.map_err(anyhow::Error::msg)?;
    let first: Vec<&str> = out.lines().filter(|l| l.contains("openapp")).take(2).collect();
    check!(code == 0 && out.contains("openapp"), "pi mcp list (Pi's own diagnostics, {} ms): {first:?}{}", ms(t), if code != 0 { format!(" stderr: {}", err.trim()) } else { String::new() });

    // 4) extension UI: ask_user → question card → answer → back to Pi
    let skip = c.notices().len();
    send_cmd(&c, "/openapp-check ask");
    let q = open_item(&k, "question", Duration::from_secs(15)).await.expect("question card");
    let labels: Vec<&str> = q["questions"][0]["options"].as_array().map(|a| a.iter().filter_map(|o| o["label"].as_str()).collect()).unwrap_or_default();
    check!(q["questions"][0]["id"] == "smoke" && labels == ["Alpha", "Beta"] && q["questions"][0]["isOther"] == true, "extension_ui_request (input, ask_user marker) → question card {:?} {labels:?}", q["questions"][0]["header"]);
    let mut a = Map::new();
    a.insert("smoke".into(), json!(["Beta"]));
    support::answer(|| k.answer_question(q["id"].as_str().unwrap(), Some(a.clone()))).await.map_err(anyhow::Error::msg)?;
    let back = notice(&c, "openapp-check ask", skip, Duration::from_secs(10)).await;
    check!(back.as_deref() == Some(r#"{"smoke":["Beta"]}"#), "answer reached Pi as extension_ui_response: {back:?}");
    // cancel path
    let skip = c.notices().len();
    send_cmd(&c, "/openapp-check ask");
    let q = open_item(&k, "question", Duration::from_secs(15)).await.expect("question card");
    support::answer(|| k.answer_question(q["id"].as_str().unwrap(), None)).await.map_err(anyhow::Error::msg)?;
    let back = notice(&c, "openapp-check ask", skip, Duration::from_secs(10)).await;
    check!(back.as_deref() == Some("null"), "cancelled card → cancelled: true → {back:?}");
    // confirm → permission card
    let skip = c.notices().len();
    send_cmd(&c, "/openapp-check confirm");
    let p = open_item(&k, "permission", Duration::from_secs(15)).await.expect("permission card");
    support::answer(|| k.respond_permission(p["id"].as_str().unwrap(), Some("allow".into()))).await.map_err(anyhow::Error::msg)?;
    let back = notice(&c, "openapp-check confirm", skip, Duration::from_secs(10)).await;
    check!(back.as_deref() == Some("true"), "confirm → permission card {:?} → confirmed {back:?}", p["title"]);

    // 5) Pi sessions: new_session gives a new id; switch_session goes back
    let a = c.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
    c.request(json!({ "type": "new_session" }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
    let b = c.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await.map_err(anyhow::Error::msg)?;
    check!(a["sessionId"] != b["sessionId"] && b["sessionFile"].is_string(), "new_session: {} → {}", a["sessionId"], b["sessionId"]);

    // 6) not signed in → a turn is refused with a clear message (no provider request)
    let sid = k.active_session();
    openapp_lib::harness::run_turn(k.clone(), sid.clone(), "hello".into(), false).await;
    let last = k.snapshot().chat.last().cloned().unwrap_or_default();
    check!(last["kind"] == "error" && last["text"].as_str().is_some_and(|t| t.contains("not signed in")), "turn while not signed in: {:?}", last["text"]);

    // 7) OAuth start: forwarded callback port, auth URL, paste accepted; cancelled (no browser, no sign-in)
    let t = Instant::now();
    let l = openapp_lib::pi::login(&k, "chatgpt", false).await.map_err(anyhow::Error::msg)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut card = None;
    while Instant::now() < deadline {
        let h = k.snapshot().harnesses.into_iter().find(|h| h.id == "pi").unwrap();
        if h.login.as_ref().is_some_and(|x| x["state"] == "browser") {
            card = h.login;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let card = card.unwrap_or_default();
    let url = card["url"].as_str().unwrap_or("");
    let port = l["port"].as_u64().unwrap_or(0) as u16;
    // the guest callback server answers through the host forward (a request without code/state)
    let resp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok();
    let mut answered = String::new();
    if let Some(mut s) = resp {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = s.write_all(b"GET /auth/callback HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
        let mut buf = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.ok().and_then(|r| r.ok()).unwrap_or(0);
        answered = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
    }
    check!(
        url.starts_with("https://auth.openai.com/") && card["paste"] == true && answered.starts_with("HTTP/1.1"),
        "ChatGPT sign-in: auth URL ready, paste field on, host 127.0.0.1:{port} → guest callback answers ({answered:?}, {} ms)",
        ms(t)
    );
    let bad = openapp_lib::pi::login_submit(&k, "code", "not a url", None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    openapp_lib::pi::login_cancel(&k);
    let h = k.snapshot().harnesses.into_iter().find(|h| h.id == "pi").unwrap();
    check!(bad.is_ok() && h.login.as_ref().is_some_and(|x| x["state"] == "required") && h.signed_in == Some(false), "pasted input forwarded to the helper; cancel → card back to the choices");

    // 8) a full turn with a FAKE key: prompt accepted, provider error reaches chat, turn ends
    if std::env::var("OPENAPP_SMOKE_PI_FAKE_KEY").map(|v| v != "0").unwrap_or(true) {
        openapp_lib::pi::login_submit(&k, "apikey", "sk-openapp-smoke-not-a-real-key", Some("openai")).await.map_err(anyhow::Error::msg)?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        wait_ready(&k, Duration::from_secs(120)).await;
        let h = k.snapshot().harnesses.into_iter().find(|h| h.id == "pi").unwrap();
        check!(h.signed_in == Some(true) && h.note.as_deref().is_some_and(|n| n.starts_with("API key: OpenAI")), "API key stored in the VM (auth.json, 0600), Pi restarted: {:?}", h.note);
        // §23: Pi's models were discovered at start (the extension's `/openapp-models`), the live list was tried
        // with Pi's own (fake) credential, and the chosen model is a discovered one (no literal)
        let disc = research(&k.state_dir).into_iter().rev().find(|r| r["kind"] == "models_discovered" && r["data"]["harness"] == "pi").map(|r| r["data"].clone()).unwrap_or_default();
        let snap = k.snapshot();
        let ids: Vec<String> = snap.models.iter().filter(|m| m["harness"] == "pi").filter_map(|m| m["id"].as_str().map(String::from)).collect();
        println!("pi discovery: source {} count {} error {}", disc["source"], disc["count"], disc["error"]);
        check!(
            disc["source"].as_str().is_some_and(|s| s.contains("openai:")) && disc["error"].as_str().is_some_and(|e| e.contains("openai-models")) && !ids.is_empty()
                && snap.agent.model.as_deref().is_some_and(|m| ids.iter().any(|i| i == m)),
            "§23 discovery in the VM: {} models, live list tried with Pi's credential ({}), current {:?}", ids.len(), disc["error"], snap.agent.model
        );
        let sid = k.session_new();
        let t = Instant::now();
        let before = k.snapshot().chat.len();
        tokio::time::timeout(Duration::from_secs(180), openapp_lib::harness::run_turn(k.clone(), sid.clone(), "Say hi.".into(), false)).await.ok();
        let chat = k.snapshot().chat;
        let errs: Vec<String> = chat.iter().skip(before).filter(|i| i["kind"] == "error").map(|i| i["text"].as_str().unwrap_or("").chars().take(160).collect()).collect();
        let stored = k.agent_session_id(&sid, "pi");
        let end = research(&k.state_dir).into_iter().rev().find(|r| r["kind"] == "turn_end").map(|r| r["data"].clone()).unwrap_or_default();
        check!(!errs.is_empty() && stored.is_some() && end["session"] == sid.as_str(), "turn ran on a Pi session ({}) and ended in {} ms with the provider's error in chat: {errs:?}", stored.unwrap_or_default(), ms(t));
        tokio::time::sleep(Duration::from_millis(1500)).await; // egress entries are written when a connection closes
        let egress: Vec<String> = research(&k.state_dir)
            .into_iter()
            .filter(|r| r["kind"] == "egress" && r["data"]["actor"] == "agent")
            .map(|r| format!("{} {}", r["data"]["host"].as_str().unwrap_or(""), r["data"]["decision"].as_str().unwrap_or("")))
            .collect();
        println!("pi egress (actor agent): {egress:?}");
        // second session, then back: switch_session to the stored file
        let sid2 = k.session_new();
        tokio::time::timeout(Duration::from_secs(180), openapp_lib::harness::run_turn(k.clone(), sid2.clone(), "Say hi again.".into(), false)).await.ok();
        let f1 = k.agent_session_id(&sid, "pi");
        let f2 = k.agent_session_id(&sid2, "pi");
        check!(f1.is_some() && f2.is_some() && f1 != f2, "each OpenApp session has its own Pi session file");
        let (_, _, _) = vm.sh(&format!("rm -f {}/auth.json", openapp_lib::pi::VM_PI_HOME), &[]).await.map_err(anyhow::Error::msg)?;
        println!("fake key removed from the scratch VM state");
    }

    // 9) Pi's own tools in the guest (fd for find) and failed tool chips: a local fake provider in the VM
    //    (OpenAI-compatible, 127.0.0.1:9911, not a credential) asks for `find` and for `read` of a missing file
    if std::env::var("OPENAPP_SMOKE_PI_TOOLS").map(|v| v != "0").unwrap_or(true) {
        let (code, out, err) = vm.sh("fd --version && rg --version | head -1", &[]).await.map_err(anyhow::Error::msg)?;
        check!(code == 0 && out.lines().next().is_some_and(|l| l.starts_with("fd")) && out.contains("ripgrep"), "guest tools (fd, rg): {} {}", out.trim().replace('\n', " · "), err.trim());
        let home = openapp_lib::pi::VM_PI_HOME;
        let setup = format!(
            r#"set -e
mkdir -p /workspace/smoke-find/sub && echo a > /workspace/smoke-find/alpha.md && echo b > /workspace/smoke-find/sub/beta.md
rm -f /tmp/fake-llm.log
cat > /tmp/fake-llm.mjs <<'JS'
{FAKE_LLM}
JS
[ -f {home}/models.json ] && cp {home}/models.json {home}/models.json.smoke-bak || true
[ -f {home}/auth.json ] && cp {home}/auth.json {home}/auth.json.smoke-bak || true
cat > {home}/models.json <<'J'
{{ "providers": {{ "fakeco": {{ "baseUrl": "http://127.0.0.1:9911/v1", "api": "openai-completions", "apiKey": "openapp-smoke-local",
  "models": [{{ "id": "fake-tools", "name": "Fake tools", "reasoning": false, "input": ["text"],
  "cost": {{ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }}, "contextWindow": 128000, "maxTokens": 4096 }}] }} }} }}
J
printf '%s' '{{"fakeco":{{"type":"api_key","key":"openapp-smoke-local"}}}}' > {home}/auth.json && chmod 600 {home}/auth.json"#
        );
        let (code, _, err) = vm.sh(&setup, &[]).await.map_err(anyhow::Error::msg)?;
        check!(code == 0, "fake provider set up in the scratch VM: {}", err.trim());
        let _server = vm.spawn("node", &["/tmp/fake-llm.mjs".into()], &[], Some("/tmp")).await.map_err(anyhow::Error::msg)?;
        let probe = "for i in $(seq 1 100); do curl -s -o /dev/null --noproxy '*' http://127.0.0.1:9911/ && exit 0; sleep 0.05; done; exit 1";
        let (code, _, _) = vm.sh(probe, &[]).await.map_err(anyhow::Error::msg)?;
        let (_, nl, _) = vm.sh("cat /tmp/fake-llm.err 2>/dev/null; true", &[]).await.map_err(anyhow::Error::msg)?;
        check!(code == 0, "fake provider listening in the guest {}", nl.trim());
        openapp_lib::harness::restart(&k);
        tokio::time::sleep(Duration::from_millis(300)).await;
        wait_ready(&k, Duration::from_secs(120)).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut chosen = Err("no catalog".to_string());
        while Instant::now() < deadline {
            chosen = openapp_lib::primitives::models::set_choice(&k, "chat", Some("fakeco/fake-tools"), None, "user").map(|r| r.model.id);
            if chosen.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        check!(chosen.as_deref() == Ok("fakeco/fake-tools"), "fake model picked: {chosen:?}");
        let sid = k.session_new();
        let before = k.snapshot().chat.len();
        let t = Instant::now();
        tokio::time::timeout(Duration::from_secs(180), openapp_lib::harness::run_turn(k.clone(), sid.clone(), "Find the markdown files.".into(), false)).await.ok();
        let chat: Vec<Value> = k.snapshot().chat.into_iter().skip(before).filter(|i| i["kind"] == "tool").collect();
        let find = chat.iter().find(|i| i["title"].as_str().is_some_and(|t| t.starts_with("Find")));
        let read = chat.iter().find(|i| i["title"].as_str().is_some_and(|t| t.starts_with("Read")));
        let (_, log, _) = vm.sh("cat /tmp/fake-llm.log 2>/dev/null", &[]).await.map_err(anyhow::Error::msg)?;
        let (_, _, _) = vm
            .sh(&format!("rm -f {home}/models.json {home}/auth.json; [ -f {home}/models.json.smoke-bak ] && mv {home}/models.json.smoke-bak {home}/models.json; [ -f {home}/auth.json.smoke-bak ] && mv {home}/auth.json.smoke-bak {home}/auth.json; rm -rf /workspace/smoke-find; true"), &[])
            .await
            .map_err(anyhow::Error::msg)?;
        println!("fake provider removed from the scratch VM state");
        let find_result = log.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).flat_map(|j| j["messages"].as_array().cloned().unwrap_or_default())
            .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "call_find").map(|m| m["content"].to_string()).next().unwrap_or_default();
        check!(
            find.is_some_and(|f| f["status"] == "completed") && find_result.contains("alpha.md") && find_result.contains("beta.md"),
            "Pi's find tool ran fd in the guest ({} ms): chip {:?}, result {}", ms(t), find.map(|f| f["status"].clone()), find_result.chars().take(160).collect::<String>()
        );
        let out = read.and_then(|r| r["output"].as_str()).unwrap_or("");
        check!(
            read.is_some_and(|r| r["status"] == "failed") && !out.trim().is_empty(),
            "failed Pi tool chip explains itself: {:?} output {:?}", read.map(|r| r["title"].clone()), out.chars().take(160).collect::<String>()
        );
    }

    let t = Instant::now();
    k.shutdown();
    println!("TIMING shutdown (VM stop): {} ms; total {} s", ms(t), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ channels_vm: §21 G1 channels over the VM WebSocket

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn channels_vm() {
    isolated("channels_vm", Opts { env: CHANNELS_ENV, vm: true, keychain: false }, channels_vm_case);
}

async fn channels_vm_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    let (k, _sink) = fixtures::channels_kernel(&ws).await?;
    anyhow::ensure!(wait_for(10, || support::status(&k, "viewmod") == "running").await, "viewmod (visible placement) did not start");

    // ---- VM: module server publishes 1000 messages over the WebSocket ----
    let _vm = vm_up(&k).await?;
    write_module(&ws, "pub", json!({ "id": "pub", "name": "Publisher", "server": { "command": ["node", "server.ts"], "env": { "ECHO": "1" } },
        "channels": { "public": ["burst"] },
        "views": { "status": { "kind": "spec", "file": "views/status.json" } }, "placements": [ { "view": "status", "anchor": "composer.above" } ] }),
        &[("server.ts", CHANNELS_SERVER_TS), ("views/status.json", r#"{ "type": "text", "value": { "$stream": "pub/burst", "path": "i", "default": "none" } }"#)])?;
    let t = Instant::now();
    let mut loaded = Err(json!(null));
    for _ in 0..40 {
        loaded = k.module_load("pub").await;
        if loaded.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let lv = loaded.unwrap_or_else(|e| e);
    check!(lv["ok"] == true && lv["runs_in"].as_str().is_some_and(|s| s.contains("VM")), "module pub loaded in the VM ({} ms)", t.elapsed().as_millis());
    let r = k.call_module_tool("pub", "ping", Default::default()).await;
    check!(r.as_ref().is_ok_and(|r| text_of(r).contains("linux")), "pub runs in linux: {:?}", r.as_ref().map(text_of));

    let mut rx = channels::subscribe(&k, "pub/burst", false);
    let collector = tokio::spawn(async move {
        let mut got: Vec<(Instant, u64)> = Vec::new();
        let end = Instant::now() + Duration::from_secs(6);
        while let Ok(Some(m)) = tokio::time::timeout_at(tokio::time::Instant::from_std(end), rx.recv()).await {
            got.push((Instant::now(), m.json().and_then(|v| v["i"].as_u64()).unwrap_or(u64::MAX)));
        }
        got
    });
    let t = Instant::now();
    let r = k.call_module_tool("pub", "burst", serde_json::from_value(json!({ "n": 1000, "topic": "pub/burst" }))?).await;
    println!("burst: {:?} ({} ms)", r.as_ref().map(text_of), t.elapsed().as_millis());
    let got = collector.await?;
    let max_window = got.iter().map(|(t0, _)| got.iter().filter(|(t1, _)| *t1 >= *t0 && t1.duration_since(*t0) < Duration::from_secs(1)).count()).max().unwrap_or(0);
    let stats = channels::hub(&k).stats().into_iter().find(|s| s["topic"] == "pub/burst").unwrap_or_default();
    check!(r.is_ok() && max_window <= 120 && got.len() < 1000 && got.last().map(|g| g.1) == Some(999),
        "1000 messages over the VM WebSocket → subscriber saw {} (max {} in any 1 s window, last i={:?}); hub {}", got.len(), max_window, got.last().map(|g| g.1), stats);
    let mut late = channels::subscribe(&k, "pub/burst", true);
    let mut replay = Vec::new();
    while let Some(m) = late.try_recv() {
        replay.push(m.json().and_then(|v| v["i"].as_u64()).unwrap_or(0));
    }
    check!(replay.len() == 64 && replay.last() == Some(&999), "ring buffer replays the last 64 to a late subscriber (last {:?})", replay.last());
    let r = k.call_module_tool("pub", "replay", serde_json::from_value(json!({ "topic": "pub/burst" }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    let rv: Value = serde_json::from_str(&r).unwrap_or_default();
    check!(rv["count"] == 64 && rv["last"]["i"] == 999, "late WebSocket subscriber in the VM gets the ring replay: {r}");
    let r = k.call_module_tool("pub", "deny", serde_json::from_value(json!({ "topic": "viewmod/counter" }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    check!(r.contains("may not publish"), "WebSocket publish on another module's topic is refused: {r}");
    let r = k.call_module_tool("pub", "post", serde_json::from_value(json!({ "topic": "pub/posted", "data": { "via": "http" } }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    check!(r.starts_with("200") && channels::hub(&k).last_json("pub/posted") == Some(json!({ "via": "http" })), "VM server POST $OPENAPP_HOST_URL/channel.publish: {r}");
    // public topic: viewmod's view may subscribe to pub/burst (listed in channels.public)
    let vp = k.snapshot().layout.placements.iter().find(|p| p.module == "viewmod").map(|p| p.id.clone()).unwrap();
    check!(channels::view_subscribe(&k, &vp, "pub/burst").is_ok(), "another module's view may subscribe to a public topic");
    // view input → module server (pub/in → pub/echo)
    let pp = k.snapshot().layout.placements.iter().find(|p| p.module == "pub").map(|p| p.id.clone()).unwrap();
    let mut echo = channels::subscribe(&k, "pub/echo", false);
    let ok = wait_for(5, || channels::hub(&k).stats().iter().any(|t| t["topic"] == "pub/in" && t["subscribers"].as_u64() > Some(0))).await;
    channels::view_publish(&k, &pp, &json!({ "topic": "pub/in", "data": { "key": "j" } })).map_err(anyhow::Error::msg)?;
    let e = tokio::time::timeout(Duration::from_secs(5), echo.recv()).await.ok().flatten();
    check!(ok && e.is_some_and(|m| m.json() == Some(&json!({ "echo": { "key": "j" } }))), "view input → module server over the WebSocket (pub/in → pub/echo)");
    let (text3, _, _) = k.snapshot_text();
    check!(text3.contains("999"), "$stream of a VM-published topic in openapp_snapshot");
    check!(wait_for(10, || lifecycle::resources(&k).get("pub").is_some_and(|r| r["where"] == "vm" && r["rss_mb"].as_f64().unwrap_or(0.0) > 1.0)).await,
        "resources: VM module {}", lifecycle::resources(&k).get("pub").cloned().unwrap_or_default());

    let t = Instant::now();
    k.shutdown();
    println!("TIMING shutdown {} ms; total {} s", t.elapsed().as_millis(), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ wave2_vm: §21 G7 secret.release in the VM

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn wave2_vm() {
    isolated("wave2_vm", Opts { env: WAVE2_ENV, vm: true, keychain: true }, wave2_vm_case);
}

async fn wave2_vm_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    let state = openapp_lib::state::state_dir(&ws);
    let (k, sink) = fixtures::wave2_kernel(&ws)?;
    k.start(false).await?;
    let c = Caller::new("agent");

    // ---- VM: G7 secret.release ----
    let vm = vm_up(&k).await?;
    let value = format!("s3cr3t-wave2d-{}", uuid::Uuid::new_v4().simple());
    k.secrets.put("wave2d-bridge", "password", value.as_bytes(), serde_json::Map::from_iter([("label".to_string(), json!("Bridge password"))])).map_err(anyhow::Error::msg)?;
    write_module(&ws, "vault", json!({ "id": "vault", "name": "Vault", "server": { "command": ["node", "server.ts"] },
        "services": [{ "name": "holder", "command": ["sh", "-c", "echo holder up; cat \"$OPENAPP_SECRETS_DIR/bridge/pw\" > /tmp/holder-saw 2>/dev/null; printf %s \"$VAULT_PW\" > /tmp/holder-env; exec sleep 3600"], "restart": "always" }] }),
        &[("server.ts", WAVE2_SERVER_TS)])?;
    let mut loaded = Err(json!(null));
    for _ in 0..40 {
        loaded = k.module_load("vault").await;
        if loaded.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let lv = loaded.unwrap_or_else(|e| e);
    check!(lv["ok"] == true && lv["runs_in"].as_str().is_some_and(|s| s.contains("VM")), "vault module server runs in the VM");
    let env_of = |k: Arc<Kernel>| async move { k.call_module_tool("vault", "env", serde_json::from_value(json!({ "name": "VAULT_PW" })).unwrap()).await.map(|r| text_of(&r)).unwrap_or_default() };
    check!(env_of(k.clone()).await == "(unset)", "before release: the server has no VAULT_PW");
    let r = prims::call_action(&k, "secret.release", json!({ "name": "wave2d-bridge", "module": "vault", "as": "env:VAULT_PW" }), c.clone()).await.unwrap();
    check!(r.is_ok(), "secret.release env (Confirmed card answered): {:?}", r);
    let card = sink.all("oa://chat").into_iter().find(|p| p["item"]["title"].as_str().is_some_and(|t| t.starts_with("Give \"Bridge password\" to Vault"))).map(|p| p["item"]["text"].clone()).unwrap_or_default();
    check!(card.as_str().is_some_and(|t| t.contains("will be able to read the value")), "release card: {card}");
    check!(env_of(k.clone()).await == value, "the VM module server sees the released value in its env");
    let r = prims::call_action(&k, "secret.release", json!({ "name": "wave2d-bridge", "module": "vault", "as": "file:bridge/pw" }), c.clone()).await.unwrap();
    let f = k.call_module_tool("vault", "readfile", serde_json::from_value(json!({ "rel": "bridge/pw" }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    let fv: Value = serde_json::from_str(&f).unwrap_or_default();
    check!(r.is_ok() && fv["text"] == value.as_str() && fv["mode"] == "600" && fv["path"].as_str().is_some_and(|p| p.starts_with("/run/openapp/secrets/vault/")), "file release: 0600 file in the VM ({})", fv["path"]);
    let mut saw = false;
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && !saw {
        saw = vm.sh("cat /tmp/holder-saw /tmp/holder-env 2>/dev/null", &[]).await.map(|(_, o, _)| o == format!("{value}{value}")).unwrap_or(false);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    check!(saw, "the module's service got the file and the env after its restart");
    let mut r2 = Err(json!(null));
    for _ in 0..20 {
        r2 = k.module_load("vault").await;
        if r2.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    check!(r2.is_ok() && env_of(k.clone()).await == value, "re-delivered after a server restart");
    let _ = vm.sh("rm -rf /run/openapp/secrets/vault", &[]).await;
    let _ = k.module_load("vault").await;
    let f = k.call_module_tool("vault", "readfile", serde_json::from_value(json!({ "rel": "bridge/pw" }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    check!(f.contains(&value), "file re-written before the server starts (e.g. a fresh VM)");
    let mut leaks = support::files_containing(&ws, &value);
    leaks.extend(support::files_containing(&state, &value));
    check!(leaks.is_empty(), "value absent from workspace logs, research log and state files ({leaks:?})");
    let r = prims::call_action(&k, "secret.revoke", json!({ "name": "wave2d-bridge", "module": "vault" }), c.clone()).await.unwrap();
    let after = env_of(k.clone()).await;
    let f = k.call_module_tool("vault", "readfile", serde_json::from_value(json!({ "rel": "bridge/pw" }))?).await.map(|r| text_of(&r)).unwrap_or_default();
    check!(r.as_ref().is_ok_and(|v| v["revoked"] == 2) && after == "(unset)" && !f.contains(&value), "revoke: env and file gone ({after})");
    let research = std::fs::read_to_string(state.join("research.jsonl")).unwrap_or_default();
    check!(research.contains("\"secret_release\"") && !research.contains(&value), "release logged in the research log without the value");

    fixtures::wave2_cleanup(&k);
    let t = Instant::now();
    k.shutdown();
    println!("TIMING shutdown {} ms; total {} s", t.elapsed().as_millis(), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ socks_vm: SOCKS5 egress (non-HTTP protocols)

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn socks_vm() {
    isolated(
        "socks_vm",
        Opts { env: &[("OPENAPP_TEST_EGRESS_LOCAL", "1"), ("OPENAPP_TEST_EGRESS_HOSTS", "sockstest.example=127.0.0.1")], vm: true, keychain: false },
        socks_vm_case,
    );
}

/// A raw RFC 1928 client (user/pass auth = actor, CONNECT by name or IPv4), then one line through the tunnel.
const SOCKS_CLIENT_PY: &str = r#"import socket, struct, sys
host, port, user = sys.argv[1], int(sys.argv[2]), sys.argv[3]
s = socket.create_connection(("127.0.0.1", 7803), 10)
s.sendall(b"\x05\x02\x00\x02"); assert s.recv(2) == b"\x05\x02"
u = user.encode(); s.sendall(bytes([1, len(u)]) + u + b"\x07openapp"); assert s.recv(2) == b"\x01\x00"
try:
    req = b"\x05\x01\x00\x01" + socket.inet_aton(host)
except OSError:
    req = b"\x05\x01\x00\x03" + bytes([len(host)]) + host.encode()
s.sendall(req + struct.pack(">H", port))
r = b""
while len(r) < 10:
    c = s.recv(10 - len(r))
    if not c: break
    r += c
print("REP", r[1])
if r[1] == 0:
    s.sendall(b"ping\n"); print(s.recv(100).decode().strip())
"#;

/// Plain sockets (no proxy code): reaches the test server only when proxychains routes it through SOCKS.
const PLAIN_CLIENT_PY: &str = r#"import socket, sys
s = socket.create_connection((sys.argv[1], int(sys.argv[2])), 10)
s.sendall(b"ping\n"); print(s.recv(100).decode().strip())
"#;

async fn socks_vm_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let t_all = Instant::now();
    // a TCP server on the Mac's loopback: one line in → "echo:<line>"; an HTTP GET → 200 "socks-ok"
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let got = String::from_utf8_lossy(&buf[..n]).to_string();
                let out = if got.starts_with("GET ") { "HTTP/1.0 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nsocks-ok".to_string() } else { format!("echo:{}", got.trim()) };
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    write_module(&ws, "sockstest", json!({ "id": "sockstest", "name": "Socks Test", "views": { "v": { "kind": "spec", "file": "views/v.json" } } }), &[("views/v.json", r#"{ "type": "text", "value": "socks" }"#)])?;
    let sink = Arc::new(support::Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    let rules: Vec<(String, &str)> = vec![
        (format!("reach localhost:{port}"), "allow_once"),
        (format!("the IP address 127.0.0.1:{port}"), "allow_workspace"),
        ("reach blocked.invalid".into(), "deny"),
        (format!("reach sockstest.example:{port}"), "allow_once"),
    ];
    auto_answer(&k, &sink, move |c| {
        let title = c["title"].as_str().unwrap_or("");
        rules.iter().find(|(h, _)| title.contains(h.as_str())).map(|(_, o)| o.to_string())
    });
    k.start(false).await?;
    let vm = vm_up(&k).await?;
    vm.write_file("/workspace/socks_client.py", SOCKS_CLIENT_PY.as_bytes()).await.map_err(anyhow::Error::msg)?;
    vm.write_file("/workspace/plain_client.py", PLAIN_CLIENT_PY.as_bytes()).await.map_err(anyhow::Error::msg)?;
    let tools = vm.guest_tools.lock().unwrap().clone();
    check!(tools["proxychains4"].is_string(), "guest image has proxychains4 ({})", tools["proxychains4"]);
    let titles = |sink: &support::Collect| sink.card_titles().into_iter().map(|(_, t)| t).collect::<Vec<_>>();

    // 1) socks5h: the guest sends the name "localhost"; the Mac resolves it (the guest's own localhost has no server)
    let (code, out, err) = vm.sh(&format!("python3 /workspace/socks_client.py localhost {port} module.sockstest"), &[]).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out.contains("REP 0") && out.contains("echo:ping"), "SOCKS5 CONNECT by name → host-side resolution reaches the Mac test server ({out:?} {err:?})");
    check!(titles(&sink).iter().any(|t| t == &format!("Socks Test wants to reach localhost:{port}")), "consent card names the module and host:port ({:?})", titles(&sink));

    // 2) deny: the card's Deny → SOCKS reply 2 (not allowed), nothing connected
    let (_, out, _) = vm.sh("python3 /workspace/socks_client.py blocked.invalid 80 agent", &[]).await.map_err(anyhow::Error::msg)?;
    check!(out.contains("REP 2"), "denied host → SOCKS reply 2 ({out:?})");

    // 3) IP literal: its own card text; Allow for this workspace stores the grant
    let (_, out, _) = vm.sh(&format!("python3 /workspace/socks_client.py 127.0.0.1 {port} module.sockstest"), &[]).await.map_err(anyhow::Error::msg)?;
    check!(out.contains("REP 0") && out.contains("echo:ping"), "SOCKS5 CONNECT to an IPv4 literal ({out:?})");
    check!(titles(&sink).iter().any(|t| t == &format!("Socks Test wants to reach the IP address 127.0.0.1:{port}")) && k.grants.allowed("network", "127.0.0.1") == Some(true), "IP-literal card + workspace grant");

    // 4) curl with the kernel's OPENAPP_SOCKS_URL (socks5h + user name); ALL_PROXY is not set
    let env = openapp_lib::vm::proxy_env("module.sockstest");
    let (code, out, err) = vm.sh(&format!("[ -z \"${{ALL_PROXY:-}}${{all_proxy:-}}\" ] && curl -sS -m 10 --noproxy '' -x \"$OPENAPP_SOCKS_URL\" http://localhost:{port}/"), &env).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out == "socks-ok", "no ALL_PROXY; curl -x $OPENAPP_SOCKS_URL (socks5h://module.sockstest:openapp@127.0.0.1:7803) works ({out:?} {err:?})");
    let (code, out, err) = vm.sh(&format!("curl -sS -m 10 --noproxy '' --socks5-hostname 127.0.0.1:7803 --proxy-user module.sockstest:openapp http://localhost:{port}/"), &env).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out == "socks-ok", "curl --socks5-hostname works ({out:?} {err:?})");

    // 5) proxychains4: a program without proxy code reaches a name only the Mac knows ("net": "socks" wrapper)
    let w = openapp_lib::vm::socks_wrap("module.sockstest", &["python3".into(), "/workspace/plain_client.py".into(), "sockstest.example".into(), port.to_string()]);
    let (code, out, err) = vm.exec_out(&w[0], &w[1..], &openapp_lib::vm::proxy_env("module.sockstest"), Some("/workspace")).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out.contains("echo:ping"), "proxychains4-wrapped plain socket reaches the test server ({out:?} {err:?})");
    let (code, out, _) = vm.exec_out("python3", &["/workspace/plain_client.py".into(), "sockstest.example".into(), port.to_string()], &[], Some("/workspace")).await.map_err(anyhow::Error::msg)?;
    check!(code != 0 && !out.contains("echo"), "without the wrapper the same program has no network");

    // 6) a manifest service with "net": "socks"
    write_module(&ws, "sockstest", json!({ "id": "sockstest", "name": "Socks Test",
        "views": { "v": { "kind": "spec", "file": "views/v.json" } },
        "services": [{ "name": "probe", "net": "socks", "restart": "never",
                       "command": ["sh", "-c", format!("python3 /workspace/plain_client.py sockstest.example {port} > /tmp/socks-svc.out 2>&1")] }] }),
        &[("views/v.json", r#"{ "type": "text", "value": "socks" }"#)])?;
    let mut loaded = Err(json!(null));
    for _ in 0..40 {
        loaded = k.module_load("sockstest").await;
        if loaded.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    check!(loaded.is_ok(), "module with a \"net\": \"socks\" service loads ({loaded:?})");
    let mut svc = String::new();
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && !svc.contains("echo:ping") {
        svc = vm.sh("cat /tmp/socks-svc.out 2>/dev/null", &[]).await.map(|(_, o, _)| o).unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    check!(svc.contains("echo:ping"), "the \"net\": \"socks\" service reached the test server ({svc:?})");

    // research log: SOCKS entries with actor and IP-literal flag
    let socks: Vec<Value> = research(&k.state_dir).into_iter().filter(|r| r["kind"] == "egress" && r["data"]["method"] == "SOCKS5").map(|r| r["data"].clone()).collect();
    check!(
        socks.iter().any(|d| d["host"] == "localhost" && d["actor"] == "module.sockstest" && d["decision"] == "allow")
            && socks.iter().any(|d| d["host"] == "blocked.invalid" && d["decision"] == "deny")
            && socks.iter().any(|d| d["host"] == "127.0.0.1" && d["ip_literal"] == true),
        "egress log: {} SOCKS5 entries with actor, decision, ip_literal", socks.len()
    );
    let run_dir = openapp_lib::vm::vm_home().join("run").join(&vm.name);
    check!(run_dir.join("socks.sock").exists() && run_dir.join("owner.pid").exists() && openapp_lib::vm::running_vms(&openapp_lib::vm::vm_home()) == vec![vm.name.clone()], "socks.sock + owner.pid while running; running_vms sees this VM");
    drop(vm);
    let t = Instant::now();
    k.shutdown();
    check!(!run_dir.join("proxy.sock").exists() && !run_dir.join("socks.sock").exists() && openapp_lib::vm::running_vms(&openapp_lib::vm::vm_home()).is_empty(), "shutdown removes the sockets: the VM home no longer looks running");
    println!("TIMING shutdown {} ms; total {} s", t.elapsed().as_millis(), t_all.elapsed().as_secs());
    println!("ALL PASS");
    Ok(())
}

// ================================================================ bulk_vm: large transfers over the vsock routes

#[test]
#[ignore = "boots the app VM (cargo test --test vm_suite -- --ignored)"]
fn bulk_vm() {
    isolated("bulk_vm", Opts { env: &[("OPENAPP_TEST_EGRESS_LOCAL", "1")], vm: true, keychain: false }, bulk_vm_case);
}

/// A Mac test server that answers only after it has read the whole request body (as an API server does):
/// `POST` → "got <n>"; `GET /<n>` → n bytes.
async fn bulk_server() -> anyhow::Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && s.read_exact(&mut b).await.is_ok() {
                    head.push(b[0]);
                }
                let h = String::from_utf8_lossy(&head).to_string();
                let len: usize = h.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                let mut body = vec![0u8; len];
                let got = if s.read_exact(&mut body).await.is_ok() { len } else { 0 };
                let out = if h.starts_with("GET /") {
                    let n: usize = h.split_whitespace().nth(1).and_then(|p| p.trim_start_matches('/').parse().ok()).unwrap_or(0);
                    vec![b'x'; n]
                } else {
                    format!("got {got}").into_bytes()
                };
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len()).as_bytes()).await;
                let _ = s.write_all(&out).await;
                let _ = s.shutdown().await;
            });
        }
    });
    Ok(port)
}

async fn bulk_vm_case(cx: Ctx) -> anyhow::Result<()> {
    let ws = cx.ws;
    let port = bulk_server().await?;
    let sink = Arc::new(support::Collect::default());
    let k = Kernel::new_opts(ws.clone(), None, None, sink.clone())?;
    let host = format!("reach localhost:{port}");
    auto_answer(&k, &sink, move |c| c["title"].as_str().unwrap_or("").contains(host.as_str()).then(|| "allow_workspace".to_string()));
    k.start(false).await?;
    let vm = vm_up(&k).await?;
    let env = openapp_lib::vm::proxy_env("agent");
    let (code, _, err) = vm.sh("head -c 3145728 /dev/zero > /tmp/big", &[]).await.map_err(anyhow::Error::msg)?;
    check!(code == 0, "3 MiB test body in the guest ({err})");
    let (code, out, err) = vm.sh(&format!("curl -sS -m 10 --noproxy '' -p -x \"$HTTPS_PROXY\" --data-binary small http://localhost:{port}/"), &env).await.map_err(anyhow::Error::msg)?;
    check!(code == 0 && out == "got 5", "small upload through HTTP CONNECT ({out:?} {err:?})");
    // uploads larger than the vsock route's 256 KiB credit; the server is silent until it has the whole body
    for (via, flags) in [("HTTP CONNECT", "-p -x \"$HTTPS_PROXY\""), ("SOCKS5", "--socks5-hostname 127.0.0.1:7803 --proxy-user agent:openapp")] {
        let t = Instant::now();
        let (code, out, err) = vm.sh(&format!("curl -sS -m 30 --noproxy '' {flags} --data-binary @/tmp/big http://localhost:{port}/"), &env).await.map_err(anyhow::Error::msg)?;
        check!(code == 0 && out == "got 3145728", "3 MiB upload through {via} in {} ms ({out:?} {err:?})", ms(t));
        let t = Instant::now();
        let (code, out, err) = vm.sh(&format!("curl -sS -m 30 --noproxy '' {flags} http://localhost:{port}/3145728 | wc -c"), &env).await.map_err(anyhow::Error::msg)?;
        check!(code == 0 && out.trim() == "3145728", "3 MiB download through {via} in {} ms ({out:?} {err:?})", ms(t));
    }
    // the MCP route: a large tool call reaches the kernel MCP server whole
    let token = k.mcp.get().map(|e| e.token.clone()).unwrap_or_default();
    let (code, out, err) = vm
        .sh(
            &format!("python3 -c 'import json,sys; sys.stdout.write(json.dumps({{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"clientInfo\":{{\"name\":\"bulk\",\"version\":\"1\",\"pad\":\"x\"*1048576}}}}}}))' > /tmp/mcp.json && curl -sS -m 30 --noproxy '*' -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data-binary @/tmp/mcp.json http://127.0.0.1:7801/mcp"),
            &[],
        )
        .await
        .map_err(anyhow::Error::msg)?;
    check!(code == 0 && out == "200", "1 MiB MCP request over the vsock route ({out:?} {err:?})");
    drop(vm);
    k.shutdown();
    println!("ALL PASS");
    Ok(())
}
