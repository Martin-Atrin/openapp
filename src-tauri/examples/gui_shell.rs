//! GUI suite with a real (hidden) Tauri app; needs a logged-in macOS GUI session and the Vite dev server
//! (`pnpm dev` on 5173: the hidden shell renders lucide tray icons for the kernel).
//! Run: `cargo run --example gui_shell` (or `node scripts/test.ts --gui`, which starts and stops Vite).
//!
//! 0. attach: the agent sets tray / menu / dock badge / a window through `openapp_host_action` before the shell is
//!    attached (results say pending); `shell_misc::attach` then applies them (tray item and window appear).
//! 1. shell (§21 G4/G5): windows, menu bar, tray, app-wide and global shortcuts, notifications (+ simulated answer),
//!    clipboard round trip with a grant, dock badge, deep links, file handlers, drops, folder grants
//!    (security-scoped bookmark on a scratch folder) and the VM mount at /mnt/<name> in a scratch VM (skipped
//!    with OPENAPP_SMOKE_NO_VM=1 or without the guest image). Print, the share sheet, real tray rendering and
//!    native panels need eyes (not tested here).
//! 2. web (§20.3): a child WKWebView: web.read (refs, text), web.fill, web.click (and the guard on "Create …"
//!    buttons), web.highlight, the per-site grant card, and a real download from the page (OAuth client JSON →
//!    card → Keychain).
//!
//! Scratch workspace, state home, VM home and Keychain prefix (`support::Scratch`), all removed at the end.

#[path = "../tests/support/mod.rs"]
#[macro_use]
mod support;

use openapp_lib::kernel::{Kernel, WebRect};
use openapp_lib::primitives::{self, Caller, files, shell, shell_misc, shell_notify};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Collect, auto_answer, first_option, write};
use tauri::Manager;

fn main() {
    let vm = std::env::var("OPENAPP_SMOKE_NO_VM").is_err() && support::guest_tar().is_file();
    let scratch = support::Scratch::new("gui_shell", vm);
    // SAFETY: before any thread starts.
    unsafe {
        for (k, v) in scratch.env() {
            std::env::set_var(k, v);
        }
        std::env::remove_var("OPENAPP_GUEST_IMAGE_TAR");
    }
    let ws = scratch.ws.clone();
    let mut ctx = tauri::generate_context!();
    ctx.config_mut().app.windows.clear();
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_snapshot, primitive_command])
        .setup(move |app| {
            // the real shell in a hidden main window (it renders lucide tray icons for the kernel; web views are its children)
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into())).title("OpenApp GUI suite").inner_size(1100.0, 800.0).visible(false).build()?;
            let sink = Arc::new(Collect::default());
            {
                use tauri::Emitter;
                let h = app.handle().clone();
                let _ = sink.forward.set(Box::new(move |e, p| {
                    let _ = h.emit(e, p.clone());
                }));
            }
            let k = Kernel::new_opts(ws.clone(), None, None, sink.clone()).map_err(|e| e.to_string())?;
            let _ = k.web_host.set(openapp_lib::web::TauriWeb::new(app.handle().clone()));
            app.manage(k.clone());
            // attached in the "attach" phase below, after desired state was stored without a shell
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let t = Instant::now();
                // consent cards: allow in the shell part; the first option (allow web control, save the OAuth client) in the web part
                let web = Arc::new(Mutex::new(false));
                {
                    let web = web.clone();
                    auto_answer(&k, &sink, move |c| if *web.lock().unwrap() { first_option(c) } else { Some("allow".into()) });
                }
                let mut failed = false;
                if let Err(e) = k.start(false).await {
                    println!("FAIL kernel start: {e}");
                    failed = true;
                }
                failed |= !phase("attach", 60, attach_part(k.clone(), ws.clone(), handle.clone())).await;
                failed |= !phase("shell", 240, shell_part(k.clone(), sink.clone(), ws.clone(), handle.clone())).await;
                *web.lock().unwrap() = true;
                failed |= !phase("web", 90, web_part(k.clone(), ws.clone())).await;
                for m in k.secrets_list() {
                    let _ = k.secrets.delete(m["name"].as_str().unwrap_or(""));
                }
                let clean = k.secrets_list().is_empty();
                println!("{} Keychain items cleaned up", if clean { "PASS" } else { "FAIL" });
                failed |= !clean;
                if let Some(vm) = k.vm() {
                    vm.stop().await;
                }
                k.shutdown();
                drop(scratch);
                println!("TIMING total {} ms", t.elapsed().as_millis());
                println!("{}", if failed { "SOME FAILED" } else { "ALL PASS" });
                handle.exit(if failed { 1 } else { 0 });
            });
            Ok(())
        })
        .build(ctx)
        .expect("tauri app");
    app.run(|h, e| shell_misc::on_run_event(h, &e));
}

/// Run one part with a time limit; a failed `check!` (panic), an error or the timeout fails it.
async fn phase(name: &str, secs: u64, part: impl std::future::Future<Output = Result<(), String>> + Send + 'static) -> bool {
    let t = Instant::now();
    let mut h = tokio::spawn(part);
    let r = tokio::time::timeout(Duration::from_secs(secs), &mut h).await;
    let ok = match r {
        Ok(Ok(Ok(()))) => true,
        Ok(Ok(Err(e))) => {
            println!("FAIL {name}: error: {e}");
            false
        }
        Ok(Err(_)) => {
            println!("FAIL {name}: a check failed (see above)");
            false
        }
        Err(_) => {
            h.abort();
            println!("FAIL {name}: timed out");
            false
        }
    };
    println!("TIMING {name} {} ms", t.elapsed().as_millis());
    ok
}

/// A dependency-free MCP server (stdio JSON-RPC) that records every tool call in data/calls.jsonl.
const SHELL_SERVER_TS: &str = r#"import * as readline from "node:readline";
import { appendFileSync, mkdirSync } from "node:fs";
mkdirSync("data", { recursive: true });
const rl = readline.createInterface({ input: process.stdin });
const send = (m: unknown) => process.stdout.write(JSON.stringify(m) + "\n");
const tools = ["record", "on_deeplink", "on_open"].map((name) => ({ name, description: name, inputSchema: { type: "object", additionalProperties: true } }));
rl.on("line", (line: string) => {
  const m = JSON.parse(line);
  if (m.method === "initialize") send({ jsonrpc: "2.0", id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "sh", version: "1" } } });
  else if (m.method === "tools/list") send({ jsonrpc: "2.0", id: m.id, result: { tools } });
  else if (m.method === "tools/call") {
    appendFileSync("data/calls.jsonl", JSON.stringify({ tool: m.params.name, args: m.params.arguments ?? {} }) + "\n");
    send({ jsonrpc: "2.0", id: m.id, result: { content: [{ type: "text", text: "ok" }] } });
  } else if (m.id !== undefined) send({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "unknown" } });
});
"#;

type K<'a> = tauri::State<'a, Arc<Kernel>>;

/// The two shell commands the hidden shell needs here (snapshot + primitive commands such as shell.icon).
#[tauri::command]
async fn get_snapshot(k: K<'_>) -> Result<openapp_lib::state::Snapshot, String> {
    Ok(k.snapshot())
}

#[tauri::command]
async fn primitive_command(k: K<'_>, name: String, args: Option<Value>, placement_id: Option<String>) -> Result<Value, String> {
    let h = primitives::command(&name).ok_or_else(|| format!("unknown primitive command \"{name}\""))?;
    let caller = match &placement_id {
        Some(p) => Caller::for_placement(k.inner(), p),
        None => Caller::new("user"),
    };
    h(k.inner().clone(), args.unwrap_or(Value::Null), caller).await.map_err(String::from)
}

fn calls(ws: &Path) -> Vec<Value> {
    std::fs::read_to_string(ws.join("modules/sh/data/calls.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

async fn wait_call(ws: &Path, pred: impl Fn(&Value) -> bool) -> Option<Value> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(8) {
        if let Some(c) = calls(ws).into_iter().find(|c| pred(c)) {
            return Some(c);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

fn view(m: &str) -> Caller {
    Caller { id: format!("view:{m}"), placement: None }
}

async fn act(k: &Arc<Kernel>, name: &str, args: Value, caller: Caller) -> Result<Value, String> {
    primitives::call_action(k, name, args, caller).await.unwrap_or_else(|| Err(format!("no action {name}")))
}


/// Desired native state stored by the agent before the shell exists is applied when it attaches.
async fn attach_part(k: Arc<Kernel>, ws: PathBuf, app: tauri::AppHandle) -> Result<(), String> {
    use primitives::connect::agent_action;
    let m = ws.join("modules/pend");
    write(&m.join("openapp.json"), r#"{ "id": "pend", "name": "Pending", "views": { "mini": { "kind": "spec", "file": "views/mini.json" } } }"#);
    write(&m.join("views/mini.json"), r#"{ "type": "text", "value": "mini player" }"#);
    k.module_load("pend").await.map_err(|e| e.to_string())?;
    let tray = agent_action(&k, "tray.set", &json!({ "module": "pend", "icon": "music", "title": "Pending", "menu": [{ "label": "Play", "action": { "host": "notify", "text": "play" } }] })).await?;
    let menu = agent_action(&k, "menu.set", &json!({ "module": "pend", "menus": [{ "title": "Pending", "items": [{ "label": "Go", "action": { "host": "notify", "text": "go" } }] }] })).await?;
    let win = agent_action(&k, "window.open", &json!({ "view": "pend/mini", "title": "Pending", "visible": false, "width": 320, "height": 200 })).await?;
    let badge = agent_action(&k, "dock.badge", &json!({ "text": "7" })).await?;
    check!(
        [&tray, &menu, &win, &badge].iter().all(|v| v["applied"] == false && v["pending"] == true) && app.tray_by_id("openapp-tray").is_none(),
        "before attach: agent tray.set / menu.set / window.open / dock.badge answer pending, no tray item yet"
    );
    let wid = win["windowId"].as_str().unwrap_or("").to_string();
    shell_misc::attach(&k, &app);
    let mut ok = false;
    for _ in 0..100 {
        ok = app.tray_by_id("openapp-tray").is_some() && app.get_webview_window(&format!("win-{wid}")).is_some();
        if ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let menus: Vec<String> = app
        .menu()
        .and_then(|m| m.items().ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.as_submenu().and_then(|s| s.text().ok()))
        .collect();
    let ns = k.describe()["native_shell"].clone();
    check!(ok && menus.iter().any(|t| t == "Pending") && ns["attached"] == true && ns["windows"][0]["pending"].is_null(), "after attach: tray item, window and menu applied (menus {menus:?}; {ns})");
    // clean up for the shell part
    for (a, args) in [
        ("tray.clear", json!({ "module": "pend" })),
        ("menu.set", json!({ "module": "pend", "menus": [] })),
        ("window.close", json!({ "id": wid })),
        ("dock.badge", json!({ "text": null })),
    ] {
        agent_action(&k, a, &args).await?;
    }
    Ok(())
}

async fn shell_part(k: Arc<Kernel>, sink: Arc<Collect>, ws: PathBuf, app: tauri::AppHandle) -> Result<(), String> {
    let m = ws.join("modules/sh");
    write(&m.join("server.ts"), SHELL_SERVER_TS);
    write(
        &m.join("openapp.json"),
        r#"{ "id": "sh", "name": "Shell test", "server": { "command": ["node", "server.ts"] },
            "views": { "panel": { "kind": "spec", "file": "views/panel.json" } },
            "deeplinks": ["open", { "prefix": "play", "action": { "tool": "record", "args": { "via": "deeplink-play" } } }],
            "opens": [".md", "audio/*"] }"#,
    );
    write(&m.join("views/panel.json"), r#"{ "type": "stack", "children": [ { "type": "text", "value": "panel" }, { "type": "dropzone", "label": "Drop here", "action": { "tool": "record", "args": { "via": "dropzone" } } }, { "type": "button", "label": "Full", "action": { "host": "window.fullscreen" } } ] }"#);
    let r = k.module_load("sh").await.map_err(|e| e.to_string())?;
    check!(r["status"] == "running", "module with a dropzone + registry host action in its spec loads: {}", r["status"]);

    // ---- windows ----
    let t = Instant::now();
    let r = act(&k, "window.open", json!({ "view": "sh/panel", "title": "Panel", "visible": false, "width": 500, "height": 400 }), view("sh")).await?;
    let wid = r["windowId"].as_str().unwrap_or("").to_string();
    check!(!wid.is_empty() && app.get_webview_window(&format!("win-{wid}")).is_some(), "window.open creates a native window ({wid}, {} ms)", t.elapsed().as_millis());
    let snap = k.snapshot();
    check!(snap.host_data["windows"].as_array().is_some_and(|a| a.iter().any(|w| w["id"] == wid.as_str() && w["view"] == "sh/panel")), "snapshot hostData.windows lists it");
    check!(k.placement_view(&format!("window:{wid}")) == Some(("sh".into(), "panel".into())), "placement window:<id> resolves to the view");
    let r = k.ui_action_value(&format!("window:{wid}"), json!({ "tool": "record", "args": { "via": "window" } }), None).await;
    check!(r.is_ok() && wait_call(&ws, |c| c["args"]["via"] == "window").await.is_some(), "an action from the window's view runs the module tool");
    check!(act(&k, "window.open", json!({ "view": "other/x" }), view("sh")).await.is_err(), "a module cannot open another module's view");
    let r = act(&k, "window.close", json!({ "id": wid }), view("sh")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    check!(r.is_ok() && app.get_webview_window(&format!("win-{wid}")).is_none() && k.snapshot().host_data["windows"].as_array().is_some_and(|a| a.is_empty()), "window.close closes it and forgets it");

    // ---- menus ----
    let r = act(&k, "menu.set", json!({ "menus": [{ "title": "Track", "items": [{ "label": "Play", "shortcut": "Cmd+Shift+P", "action": { "tool": "record", "args": { "via": "menu" } } }, "separator", { "label": "Loop", "checked": true, "action": { "tool": "record" } }, { "label": "More", "items": [{ "label": "Deep", "action": { "tool": "record" } }] }] }] }), view("sh")).await?;
    check!(r["ok"] == true && r["conflicts"].as_array().is_some_and(|c| c.is_empty()) && r["applied"] == true, "menu.set installs the menu bar: {r}");
    let menu = app.menu().ok_or("no app menu")?;
    let titles: Vec<String> = menu.items().map_err(|e| e.to_string())?.iter().filter_map(|i| i.as_submenu().and_then(|s| s.text().ok())).collect();
    check!(titles.first().map(String::as_str) == Some("OpenApp") && titles.iter().any(|t| t == "Edit") && titles.iter().any(|t| t == "Track") && titles.last().map(String::as_str) == Some("Window"), "menu bar: OpenApp, Edit, View, module menus, Window: {titles:?}");
    let play = shell::st(&k).item_actions.lock().unwrap().iter().find(|(_, (_, a))| a["args"]["via"] == "menu").map(|(id, _)| id.clone()).ok_or("no menu item id")?;
    primitives::shell_menu::on_menu_event(&k, &play);
    check!(wait_call(&ws, |c| c["args"]["via"] == "menu").await.is_some(), "a menu click runs the item's module action");
    let r = act(&k, "menu.set", json!({ "menus": [{ "title": "Bad", "items": [{ "label": "Quit?", "shortcut": "Cmd+Q", "action": { "tool": "record" } }] }] }), view("sh")).await?;
    check!(r["conflicts"].as_array().is_some_and(|c| c.len() == 1), "Cmd+Q is reported as a conflict with the OpenApp menu");
    check!(act(&k, "menu.set", json!({ "menus": [{ "title": "X", "items": [{ "label": "no action" }] }] }), view("sh")).await.is_err(), "an item without action is refused");
    act(&k, "menu.set", json!({ "menus": [] }), view("sh")).await?;

    // ---- tray ----
    let r = act(&k, "tray.set", json!({ "icon": "music", "title": "▶", "menu": [{ "label": "Toggle", "action": { "tool": "record", "args": { "via": "tray" } } }], "action": { "tool": "record", "args": { "via": "trayclick" } } }), view("sh")).await?;
    check!(r["ok"] == true && app.tray_by_id("openapp-tray").is_some(), "tray.set creates the menu-bar item: {r}");
    let t = Instant::now();
    while shell::st(&k).icons.lock().unwrap().get("music").is_none() && t.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    check!(shell::st(&k).icons.lock().unwrap().get("music").is_some_and(|p| p.starts_with(&[0x89, b'P', b'N', b'G'])), "the shell rendered the lucide icon to PNG for the tray ({} ms)", t.elapsed().as_millis());
    primitives::shell_menu::on_menu_event(&k, "oa:t:1");
    check!(wait_call(&ws, |c| c["args"]["via"] == "tray").await.is_some(), "a tray menu item runs the module action");
    check!(primitives::shell_tray::has_tray(&k), "while a tray item exists, closing the main window hides it (keeps running)");
    act(&k, "tray.clear", json!({}), view("sh")).await?;
    check!(app.tray_by_id("openapp-tray").is_none(), "tray.clear removes it");

    // ---- shortcuts ----
    let r = act(&k, "shortcut.register", json!({ "accelerator": "Space", "action": { "tool": "record", "args": { "via": "shortcut" } } }), view("sh")).await?;
    let sid = r["id"].as_str().unwrap_or("").to_string();
    let listed = k.snapshot().host_data["shortcuts"].as_array().cloned().unwrap_or_default();
    check!(listed.iter().any(|s| s["id"] == sid.as_str() && s["code"] == "Space" && s["global"] == false), "app-wide shortcut registered and listed for the shell keymap: {sid}");
    let fire = primitives::command("shortcut.fire").ok_or("no shortcut.fire")?;
    fire(k.clone(), json!({ "id": sid }), Caller::new("user")).await?;
    check!(wait_call(&ws, |c| c["args"]["via"] == "shortcut").await.is_some(), "a shortcut press (shell command) runs the action");
    check!(act(&k, "shortcut.register", json!({ "accelerator": "Cmd+Q", "action": { "tool": "record" } }), view("sh")).await.is_err(), "a kernel accelerator is refused (conflict)");
    let g = act(&k, "shortcut.register", json!({ "accelerator": "Cmd+Ctrl+Alt+Shift+F19", "global": true, "action": { "tool": "record", "args": { "via": "global" } } }), view("sh")).await;
    check!(g.as_ref().is_ok_and(|g| g["global"] == true) && sink.cards().iter().any(|c| c["title"].as_str().is_some_and(|t| t.contains("global shortcut"))), "a global shortcut asks once (card) and registers: {:?}", g.as_ref().map(|v| v["accelerator"].clone()));
    {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let reg = app.global_shortcut().is_registered("Command+Control+Alt+Shift+F19");
        check!(reg, "the global shortcut is registered with macOS");
    }
    act(&k, "shortcut.unregister", json!({ "accelerator": "Cmd+Ctrl+Alt+Shift+F19" }), view("sh")).await?;
    act(&k, "shortcut.unregister", json!({ "id": sid }), view("sh")).await?;

    // ---- notifications ----
    let r = act(&k, "notify.post", json!({ "title": "OpenApp shell smoke", "body": "Notification with a button (no need to click).", "actions": [{ "id": "open", "label": "Open" }], "action": { "tool": "record", "args": { "via": "notify" } } }), view("sh")).await;
    println!("  notify.post → {r:?}");
    let nid = r.as_ref().ok().and_then(|v| v["id"].as_str().map(String::from));
    check!(nid.is_some(), "notify.post posts (via {})", r.as_ref().map(|v| v["via"].clone()).unwrap_or_default());
    if let Some(nid) = nid {
        shell_notify::simulate_response(&k, &nid, "open").await;
        check!(wait_call(&ws, |c| c["args"]["via"] == "notify" && c["args"]["actionId"] == "open").await.is_some(), "a notification button answer routes to the module action with actionId");
    }

    // ---- clipboard (the user's clipboard is restored afterwards) ----
    {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        let saved = app.clipboard().read_text().ok();
        let text = format!("openapp-smoke-{}", std::process::id());
        act(&k, "clipboard.write", json!({ "text": text }), view("sh")).await?;
        let r = act(&k, "clipboard.read", json!({}), view("sh")).await;
        check!(r.as_ref().is_ok_and(|v| v["text"] == text.as_str()), "clipboard.write → clipboard.read round trip (read needed the module's grant)");
        check!(k.grants_list().iter().any(|g| g["id"] == "clipboard.read:sh" && g["allowed"] == true), "the clipboard grant is listed in host data grants");
        if let Some(s) = saved {
            let _ = app.clipboard().write_text(s);
        }
    }
    check!(act(&k, "dock.badge", json!({ "text": "3" }), view("sh")).await.is_ok() && act(&k, "dock.badge", json!({ "text": null }), view("sh")).await.is_ok(), "dock.badge sets and clears");

    // ---- deep links + file handlers + drops ----
    let r = shell_misc::opened(&k, "openapp://sh/open/abc?x=1").await;
    check!(r.is_ok() && wait_call(&ws, |c| c["tool"] == "on_deeplink" && c["args"]["path"] == "open/abc" && c["args"]["query"]["x"] == "1").await.is_some(), "openapp://sh/open/abc?x=1 → manifest deeplinks → on_deeplink {{ path, query }}");
    let r = shell_misc::opened(&k, "openapp://sh/play").await;
    check!(r.is_ok() && wait_call(&ws, |c| c["args"]["via"] == "deeplink-play").await.is_some(), "a deeplink prefix with its own action");
    check!(shell_misc::opened(&k, "openapp://sh/nothing").await.is_err() && shell_misc::opened(&k, "openapp://nope/x").await.is_err(), "unhandled paths / unknown modules are refused");
    let outside = ws.with_file_name(format!("{}-outside", ws.file_name().unwrap().to_string_lossy()));
    let _ = std::fs::remove_dir_all(&outside);
    write(&outside.join("note.md"), "# hello");
    write(&outside.join("song.mp3"), "ID3");
    let url = url::Url::from_file_path(outside.join("note.md")).unwrap().to_string();
    let r = shell_misc::opened(&k, &url).await;
    check!(
        r.is_ok() && ws.join("modules/sh/data/inbox/note.md").is_file() && wait_call(&ws, |c| c["tool"] == "on_open" && c["args"]["file"]["path"] == "/workspace/modules/sh/data/inbox/note.md").await.is_some(),
        "Open with OpenApp (.md) → copied into the module inbox → on_open {{ file }}"
    );
    check!(shell_misc::open_file(&k, &outside.join("song.mp3")).await.is_ok(), "\"audio/*\" matches .mp3");
    let drop = primitives::command("files.drop").ok_or("no files.drop")?;
    let r = drop(k.clone(), json!({ "paths": [outside.join("note.md")] }), view("sh")).await?;
    check!(r["files"][0]["path"] == "/workspace/modules/sh/data/inbox/note (2).md" && r["files"][0]["mime"] == "text/markdown", "a drop copies files into the inbox with unique names: {}", r["files"][0]["path"]);

    // ---- folder grants ----
    let gdir = outside.join("Granted Songs");
    write(&gdir.join("hello.txt"), "hello from the Mac");
    let r = files::grant_path(&k, &gdir, Some("songs"), "smoke test", Some("sh"), "test").await?;
    check!(r["folder"]["guest"] == "/mnt/songs", "folder grant → /mnt/songs: {}", r["folder"]);
    let store: Value = serde_json::from_slice(&std::fs::read(k.state_dir.join("folders.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    check!(store[0]["bookmark"].as_str().is_some_and(|b| b.len() > 100), "security-scoped bookmark stored in the state dir ({} b64 chars)", store[0]["bookmark"].as_str().map(str::len).unwrap_or(0));
    check!(openapp_lib::paths::map_any(&k, "/mnt/songs/hello.txt", true).is_ok_and(|p| p.ends_with("hello.txt")) && openapp_lib::paths::map_any(&k, "/mnt/songs/../note.md", true).is_err(), "/mnt/songs/… maps to the granted folder for host services (no ..)");
    check!(openapp_lib::paths::map_path(&k.workspace, "/mnt/songs/hello.txt", true).is_ok(), "host service paths accept /mnt/<grant>/…");
    check!(k.snapshot().host_data["folders"].as_array().is_some_and(|a| a.iter().any(|f| f["guest"] == "/mnt/songs")) && k.grants_list().iter().any(|g| g["id"] == "folder:songs"), "host data folders + grants list the folder");
    // rename the folder: the bookmark follows it
    let moved = outside.join("Songs Renamed");
    std::fs::rename(&gdir, &moved).map_err(|e| e.to_string())?;
    check!(files::folders(&k).iter().any(|f| f["path"].as_str().is_some_and(|p| p.ends_with("Songs Renamed"))), "the bookmark resolves the renamed folder");

    if std::env::var("OPENAPP_SMOKE_NO_VM").is_ok() {
        println!("SKIP VM mount (OPENAPP_SMOKE_NO_VM)");
    } else {
        vm_part(&k, &outside, &moved).await?;
    }
    files::revoke_name(&k, "songs", "test").await?;
    check!(files::folders(&k).is_empty(), "files.revoke_folder removes the grant");
    let _ = std::fs::remove_dir_all(&outside);
    Ok(())
}

async fn vm_part(k: &Arc<Kernel>, outside: &Path, songs: &Path) -> Result<(), String> {
    if let Err(e) = openapp_lib::vm::env_check() {
        println!("SKIP VM mount ({e})");
        return Ok(());
    }
    let t = Instant::now();
    let vm = k.ensure_vm().await?;
    println!("  scratch VM up in {} ms", t.elapsed().as_millis());
    let (code, out, err) = vm.sh("cat /mnt/songs/hello.txt; ls /mnt", &[]).await?;
    check!(code == 0 && out.starts_with("hello from the Mac"), "the granted folder is mounted in the VM at /mnt/songs: {:?} {}", out.trim(), err.trim());
    let (_, _, _) = vm.sh("echo from-vm > /mnt/songs/vm.txt", &[]).await?;
    check!(std::fs::read_to_string(songs.join("vm.txt")).is_ok_and(|s| s.trim() == "from-vm"), "the VM writes into the granted folder on the Mac");
    // a second grant restarts the VM with both mounts
    let second = outside.join("Second");
    std::fs::create_dir_all(&second).map_err(|e| e.to_string())?;
    let r = files::grant_path(k, &second, None, "second", Some("sh"), "test").await?;
    println!("  remount: {}", r["vm"]);
    let vm = k.vm().ok_or("VM not running after remount")?;
    let (_, out, _) = vm.sh("ls /mnt", &[]).await?;
    check!(r["vm"]["restarted"] == true && out.contains("songs") && out.contains("second"), "a new grant restarts the VM (stop {} ms + boot {} ms) with both mounts: {:?}", r["vm"]["stop_ms"], r["vm"]["boot_ms"], out.trim());
    let r = files::revoke_name(k, "second", "test").await?;
    let vm = k.vm().ok_or("VM not running after revoke")?;
    let (_, out, _) = vm.sh("ls /mnt", &[]).await?;
    check!(r["vm"]["restarted"] == true && !out.contains("second"), "revoking unmounts it (VM restarted): {:?}", out.trim());
    Ok(())
}

const PAGE: &str = r#"<!doctype html><html><head><title>Fake Console</title></head><body>
<h1>Credentials</h1><p>Create an OAuth client for your desktop app.</p>
<label for="n">Client name</label><input id="n" name="client_name" placeholder="Name">
<select id="t" aria-label="Application type"><option value="web">Web application</option><option value="desktop">Desktop app</option></select>
<button id="next" onclick="document.getElementById('out').textContent='next:'+document.getElementById('n').value+':'+document.getElementById('t').value">Next step</button>
<button id="create" onclick="document.getElementById('out').textContent='created'">Create</button>
<a id="dl" download="client_secret_web_smoke.json" href="data:application/json;charset=utf-8,%7B%22installed%22%3A%7B%22client_id%22%3A%22web-smoke.apps.googleusercontent.com%22%2C%22project_id%22%3A%22web-smoke%22%2C%22auth_uri%22%3A%22https%3A%2F%2Faccounts.google.com%2Fo%2Foauth2%2Fauth%22%2C%22token_uri%22%3A%22https%3A%2F%2Foauth2.googleapis.com%2Ftoken%22%2C%22client_secret%22%3A%22GOCSPX-web-smoke%22%7D%7D">Download JSON</a>
<div id="out">idle</div>
<input type="hidden" name="h" value="x"><div style="display:none"><button>Hidden</button></div>
</body></html>"#;

async fn web_part(k: Arc<Kernel>, ws: PathBuf) -> Result<(), String> {
    let app = axum::Router::new().route("/", axum::routing::get(|| async { axum::response::Html(PAGE) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    tokio::spawn(async move { axum::serve(l, app).await });

    let m = ws.join("modules/console");
    std::fs::create_dir_all(&m).map_err(|e| e.to_string())?;
    std::fs::write(m.join("openapp.json"), format!(r#"{{ "id": "console", "name": "Fake console", "views": {{ "web": {{ "kind": "web", "url": "http://127.0.0.1:{port}/", "profile": "openapp-web-smoke" }} }} }}"#)).map_err(|e| e.to_string())?;
    k.module_load("console").await.map_err(|e| e.to_string())?;
    let r = openapp_lib::primitives::connect::agent_action(&k, "view.open", &json!({ "view": "console/web", "as": "sheet", "title": "Console" })).await?;
    let pid = format!("present:{}", r["presented"]["id"].as_str().unwrap_or(""));
    check!(k.web_view_of(&pid).is_some(), "web view in a sheet resolves ({pid})");
    // what the shell does for a visible sheet
    k.web_bounds(&pid, Some(WebRect { x: 300.0, y: 40.0, width: 760.0, height: 720.0 }))?;
    let t = Instant::now();
    loop {
        let w = k.snapshot().web.get(&pid).cloned().unwrap_or_default();
        if w["loading"] == false && w["title"] == "Fake Console" {
            break;
        }
        if t.elapsed() > Duration::from_secs(20) {
            return Err(format!("page did not load: {w}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check!(true, "child webview created and loaded the page ({} ms)", t.elapsed().as_millis());

    let r = openapp_lib::primitives::connect::call(&k, "web.read", &json!({}), "agent").await?;
    let els = r["elements"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| els.iter().find(|e| e["name"] == name).and_then(|e| e["ref"].as_str().map(String::from));
    let names: Vec<&str> = els.iter().filter_map(|e| e["name"].as_str()).collect();
    check!(r["title"] == "Fake Console" && r["text"].as_str().is_some_and(|t| t.contains("Create an OAuth client")) && !names.contains(&"Hidden"), "web.read: title, text, {} visible elements {names:?}", els.len());
    let (Some(input), Some(select), Some(next), Some(create), Some(dl)) = (find("Client name"), find("Application type"), find("Next step"), find("Create"), find("Download JSON")) else {
        return Err(format!("missing refs in {els:?}"));
    };
    check!(els.iter().any(|e| e["role"] == "textbox" && e["ref"] == input.as_str()) && els.iter().any(|e| e["role"] == "combobox" && e["options"].as_array().is_some_and(|o| o.len() == 2)), "roles + names from labels (textbox via <label>, combobox via aria-label with options)");
    let grants = k.grants_list();
    check!(grants.iter().any(|g| g["id"] == "web.control:127.0.0.1"), "per-site grant web.control:127.0.0.1 stored after the card");
    openapp_lib::primitives::connect::call(&k, "web.fill", &json!({ "ref": input, "value": "OpenApp" }), "agent").await?;
    openapp_lib::primitives::connect::call(&k, "web.fill", &json!({ "ref": select, "value": "Desktop app" }), "agent").await?;
    openapp_lib::primitives::connect::call(&k, "web.click", &json!({ "ref": next }), "agent").await?;
    let r = openapp_lib::primitives::connect::call(&k, "web.read", &json!({}), "agent").await?;
    check!(r["text"].as_str().is_some_and(|t| t.contains("next:OpenApp:desktop")), "web.fill (input + select) and web.click took effect: {:?}", r["text"].as_str().map(|t| t.lines().last().unwrap_or("").to_string()));
    let r2 = openapp_lib::primitives::connect::call(&k, "web.read", &json!({}), "agent").await?;
    check!(r2["elements"].as_array().and_then(|a| a.iter().find(|e| e["name"] == "Next step")).map(|e| e["ref"].clone()) == Some(json!(next)), "refs are stable across reads");
    let r = openapp_lib::primitives::connect::call(&k, "web.click", &json!({ "ref": create }), "agent").await;
    check!(r.as_ref().is_err_and(|e| e.contains("for the user to click")), "web.click on \"Create\" is refused: {:?}", r.err());
    let r = openapp_lib::primitives::connect::call(&k, "web.highlight", &json!({ "ref": create, "note": "Click Create" }), "agent").await;
    check!(r.is_ok(), "web.highlight {r:?}");
    let r = openapp_lib::primitives::connect::call(&k, "web.read", &json!({}), "agent").await?;
    check!(r["text"].as_str().is_some_and(|t| t.contains("Click Create") && !t.contains("created")), "highlight note shown, Create not clicked");
    let r = openapp_lib::primitives::connect::call(&k, "web.highlight", &json!({ "clear": true }), "agent").await;
    check!(r.is_ok(), "web.highlight clear");
    let r = openapp_lib::primitives::connect::call(&k, "web.click", &json!({ "ref": "e9999" }), "agent").await;
    check!(r.as_ref().is_err_and(|e| e.contains("no element")), "unknown ref → clear error");

    // a real download from the page
    openapp_lib::primitives::connect::call(&k, "web.click", &json!({ "ref": dl }), "agent").await?;
    let t = Instant::now();
    while k.secrets.meta("google-client").is_none() && t.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let meta = k.secrets.meta("google-client").unwrap_or_default();
    check!(meta["kind"] == "oauth-client" && meta["project_id"] == "web-smoke" && meta["provider"] == "google", "download from the web view captured as oauth-client google-client ({} ms)", t.elapsed().as_millis());
    let leftovers: Vec<_> = std::fs::read_dir(k.state_dir.join("downloads-tmp")).map(|r| r.flatten().count()).into_iter().collect();
    check!(leftovers.iter().all(|n| *n == 0) && !ws.join("downloads").exists(), "the client JSON never landed in the workspace (temp file removed)");
    openapp_lib::primitives::connect::agent_action(&k, "view.close", &json!({})).await?;
    Ok(())
}
