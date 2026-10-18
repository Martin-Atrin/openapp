//! Real Codex app-server smoke test (no Tauri UI).
//! Run: `OPENAPP_SMOKE_DIR=<fresh dir> cargo run --example codex_smoke`
//! Steps: describe round trip, a real construction run, kernel-side asserts.
//! Approval cards are auto-approved with the first allow option (and printed).

#[path = "../eval/agent_run.rs"]
mod agent_run;

use openapp_lib::kernel::Kernel;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BUILD_PROMPT: &str = "Build a module 'hello' with a spec icon-button (icon sparkles) in composer.actions that calls a tool which returns a notify directive 'Hello from OpenApp'. Load it and verify with openapp_snapshot.";

fn find_any_action(tree: &Value) -> Option<Value> {
    if let Some(a) = tree.get("action").filter(|a| a.get("tool").is_some()) {
        return Some(a.clone());
    }
    tree.get("children").and_then(Value::as_array)?.iter().find_map(find_any_action)
}

fn find_action(tree: &Value) -> Option<Value> {
    if tree.get("type").and_then(Value::as_str) == Some("icon-button") {
        return Some(json!({ "icon": tree["icon"], "action": tree["action"] }));
    }
    tree.get("children").and_then(Value::as_array)?.iter().find_map(find_action)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ws: PathBuf = std::env::var_os("OPENAPP_SMOKE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("openapp-codex-smoke"));
    let sink = Arc::new(agent_run::Print::default());
    let t_all = Instant::now();
    let k = Kernel::new_opts(ws.clone(), None, Some("codex"), sink.clone())?;
    println!("workspace: {}", k.workspace.display());
    k.start(true).await?;
    let ready = agent_run::wait_ready(&k, Duration::from_secs(120)).await;
    println!("== ready: {ready:?}");
    if ready.is_err() {
        std::process::exit(2);
    }

    println!("== run 1: describe");
    let r1 = agent_run::run_prompt(&k, "Call openapp_describe and reply with the workspace path only.", Duration::from_secs(600)).await;
    let text1 = agent_run::last_agent_text(&k);
    println!("== run 1 result: {r1:?}; answer: {text1:?}");
    let ok1 = text1.contains(k.workspace.to_string_lossy().as_ref());

    println!("== run 2: construction");
    // chat mode first: the agent must not change the app
    let r2a = agent_run::run_prompt(&k, BUILD_PROMPT, Duration::from_secs(900)).await;
    let chat_reply = agent_run::last_agent_text(&k);
    println!("== run 2a (chat mode) result: {r2a:?}; answer: {chat_reply:?}; modules: {:?}", k.snapshot().modules.iter().map(|m| m.id.clone()).collect::<Vec<_>>());
    let r2 = agent_run::run_prompt(&k, &format!("/build {BUILD_PROMPT}"), Duration::from_secs(1500)).await;
    println!("== run 2 result: {r2:?}; answer: {:?}", agent_run::last_agent_text(&k));

    let snap = k.snapshot();
    let module = snap.modules.iter().find(|m| m.id == "hello").cloned();
    let placement = snap.layout.placements.iter().find(|p| p.module == "hello" && p.anchor == "composer.actions").cloned();
    println!("   module hello: {module:?}");
    println!("   placement: {placement:?}");
    let mut notify_ok = false;
    if let Some(p) = &placement {
        let view = snap.views.get(&format!("hello/{}", p.view)).cloned().unwrap_or(Value::Null);
        let found = find_action(&view["tree"]);
        println!("   icon-button: {found:?}");
        if let Some(f) = found {
            let before = sink.events.lock().unwrap().len();
            let res = k.ui_action(&p.id, f["action"].clone(), None).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let notes: Vec<Value> = sink.events.lock().unwrap()[before..]
                .iter()
                .filter(|(e, _)| e == "oa://notify")
                .map(|(_, v)| v.clone())
                .collect();
            println!("   ui_action: {res:?}; notify events: {notes:?}");
            notify_ok = res.is_ok() && notes.iter().any(|n| n["text"] == "Hello from OpenApp");
        }
    }
    let running = module.as_ref().is_some_and(|m| m.status == "running");

    println!("== run 3: build mode");
    let before = k.snapshot().chat.len();
    let r3 = agent_run::run_prompt(&k, "/build add a module 'hi' with a composer button that notifies 'hi'", Duration::from_secs(1500)).await;
    println!("== run 3 result: {r3:?}; answer: {:?}", agent_run::last_agent_text(&k));
    let snap3 = k.snapshot();
    println!("   mode: {}", snap3.active_mode);
    let wsp = k.workspace.to_string_lossy().to_string();
    let mut outside = Vec::new();
    for i in &snap3.chat[before.min(snap3.chat.len())..] {
        if i["kind"] != "tool" {
            continue;
        }
        let cmd = i["command"].as_str().unwrap_or("");
        let cwd = i["cwd"].as_str().unwrap_or("");
        if !cmd.is_empty() {
            println!("   cmd [{}] exit={} cwd={} :: {}", i["status"], i["exitCode"], cwd.replace(&wsp, "<ws>"), cmd.replace(&wsp, "<ws>").chars().take(140).collect::<String>());
        } else {
            println!("   tool [{}] {}", i["status"], i["title"]);
        }
        let probe = cmd.replace(&wsp, "<ws>");
        if (!cwd.is_empty() && !cwd.starts_with(&wsp))
            || probe.contains("~/")
            || probe.contains("/Users/")
            || probe.contains("$HOME")
            || probe.contains(".codex")
            || probe.split_whitespace().any(|w| w.trim_matches(|c| c == '\'' || c == '"') == "ps")
        {
            outside.push(cmd.to_string());
        }
    }
    let hi = snap3.modules.iter().find(|m| m.id == "hi").map(|m| m.status.clone());
    let hi_place = snap3.layout.placements.iter().find(|p| p.module == "hi").cloned();
    let mut hi_notify = false;
    if let Some(p) = &hi_place {
        let view = snap3.views.get(&format!("hi/{}", p.view)).cloned().unwrap_or(Value::Null);
        if let Some(f) = find_any_action(&view["tree"]) {
            let before = sink.events.lock().unwrap().len();
            let res = k.ui_action(&p.id, f.clone(), None).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let notes: Vec<Value> = sink.events.lock().unwrap()[before..].iter().filter(|(e, _)| e == "oa://notify").map(|(_, v)| v.clone()).collect();
            println!("   hi ui_action {f}: {res:?}; notify: {notes:?}");
            hi_notify = res.is_ok() && notes.iter().any(|n| n["text"].as_str().is_some_and(|t| t.to_lowercase().contains("hi")));
        }
    }
    println!("== run 3 SUMMARY mode={} module_hi={hi:?} placement={:?} notify={hi_notify} outside_workspace={outside:?}", snap3.active_mode, hi_place.map(|p| p.anchor));
    println!(
        "== SUMMARY run1_ok={ok1} ({r1:?}) run2={r2:?} module_running={running} placement={} notify={notify_ok} total={:?}",
        placement.is_some(),
        t_all.elapsed()
    );
    k.shutdown();
    tokio::time::sleep(Duration::from_millis(500)).await;
    if !(ok1 && running && placement.is_some() && notify_ok) {
        std::process::exit(1);
    }
    Ok(())
}
