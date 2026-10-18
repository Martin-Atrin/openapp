//! §23 "Models are discovered, never hardcoded": the kernel against a fake Codex app-server (`model/list`) and a fake
//! Pi RPC process (`/openapp-models`), both with obviously fake model ids. Proves that the catalog comes from the
//! harness, the default is the harness's own (`isDefault`), choices and efforts are applied per turn (Codex per-turn
//! overrides, Pi `set_model` + `set_thinking_level`), escalation runs on the strong model and reverts, a refused model
//! falls back, and that no model id literal is left in the kernel, the Pi templates or the shell.
//!
//! Scratch state only: OPENAPP_STATE_HOME / OPENAPP_VM_HOME point into a temp dir that is removed at the end.

#[path = "../examples/eval/agent_run.rs"]
mod agent_run;

use openapp_lib::kernel::Kernel;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn log_lines(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn turn_starts(p: &Path) -> Vec<(String, String)> {
    log_lines(p)
        .into_iter()
        .filter(|v| v["method"] == "turn/start")
        .map(|v| (v["params"]["model"].as_str().unwrap_or("").to_string(), v["params"]["effort"].as_str().unwrap_or("").to_string()))
        .collect()
}

/// Pi commands of a kind: set_model → "provider/id", set_thinking_level → level.
fn pi_calls(p: &Path, ty: &str) -> Vec<String> {
    log_lines(p)
        .into_iter()
        .filter(|v| v["type"] == ty)
        .map(|v| match ty {
            "set_model" => format!("{}/{}", v["provider"].as_str().unwrap_or(""), v["modelId"].as_str().unwrap_or("")),
            _ => v["level"].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

fn catalog_ids(k: &Kernel, harness: &str) -> Vec<String> {
    k.snapshot().models.iter().filter(|m| m["harness"] == harness).filter_map(|m| m["id"].as_str().map(String::from)).collect()
}

async fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn agent(k: &Kernel) -> (String, String, String) {
    let a = k.snapshot().agent;
    (a.model.unwrap_or_default(), a.effort.unwrap_or_default(), a.tier.unwrap_or_default())
}

/// Send a chat command and give the kernel a moment.
async fn cmd(k: &Arc<Kernel>, text: &str) {
    k.send_prompt(text.to_string());
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// A turn that calls `openapp_model_request` (policy auto) while it runs.
async fn escalating_turn(k: &Arc<Kernel>, prompt: &str) -> Value {
    k.send_prompt(prompt.to_string());
    wait_for("the turn to run", Duration::from_secs(20), || k.running_session().is_some()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = openapp_lib::primitives::call_tool(k, "openapp_model_request", json!({ "tier": "strong", "reason": "fake hard task" })).await.expect("tool exists").expect("tool ok");
    wait_for("the task to end", Duration::from_secs(30), || k.running_session().is_none() && k.agent_status().0 != "working").await;
    r
}

/// Model id literals of real providers (the kernel discovers models; it never names one).
fn literal_hits(root: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![root.join("src-tauri/src"), root.join("workspace-template"), root.join("src")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.extension().and_then(|x| x.to_str()).is_some_and(|x| ["rs", "ts", "tsx", "md", "json"].contains(&x)) {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            for pat in ["gpt-", "claude-opus-", "claude-sonnet-", "claude-haiku-", "claude-fable-", "o3-mini", "o4-mini", "gemini-1", "gemini-2", "gemini-3"] {
                let mut from = 0;
                while let Some(i) = lower[from..].find(pat) {
                    let at = from + i + pat.len();
                    let next = lower[at..].chars().next();
                    if !pat.ends_with('-') || next.is_some_and(|c| c.is_ascii_digit()) {
                        hits.push(format!("{}: {}", p.display(), &text[from + i..(at + 6).min(text.len())]));
                    }
                    from = at;
                }
            }
        }
    }
    hits
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_are_discovered_and_applied() {
    let root = std::env::temp_dir().join(format!("openapp-models-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let fakes = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fakes");
    // SAFETY: this test binary has one test; the env is set before any kernel thread reads it.
    unsafe {
        std::env::set_var("OPENAPP_STATE_HOME", root.join("state"));
        std::env::set_var("OPENAPP_VM_HOME", root.join("vm"));
        std::env::remove_var("OPENAPP_HARNESS");
    }

    // ---------- Codex (fake app-server through the test transport) ----------
    let codex_log = root.join("codex.jsonl");
    unsafe {
        std::env::set_var("OPENAPP_CODEX_CMD", format!("node {}", fakes.join("fake_codex.ts").display()));
        std::env::set_var("OPENAPP_FAKE_LOG", &codex_log);
    }
    let k = Kernel::new_opts(root.join("ws-codex"), None, Some("codex"), Arc::new(agent_run::Print::default())).unwrap();
    k.start(true).await.unwrap();
    agent_run::wait_ready(&k, Duration::from_secs(60)).await.unwrap();
    wait_for("the Codex catalog", Duration::from_secs(20), || !catalog_ids(&k, "codex").is_empty()).await;
    assert_eq!(catalog_ids(&k, "codex"), ["fake-alpha", "fake-beta", "fake-gamma"], "model/list, both pages, no hidden model");
    assert_eq!(agent(&k), ("fake-beta".into(), "medium".into(), "default".into()), "no choice: the harness default (isDefault) with its default effort");

    agent_run::run_prompt(&k, "hello", Duration::from_secs(30)).await.unwrap();
    assert_eq!(turn_starts(&codex_log).last().unwrap(), &("fake-beta".to_string(), "medium".to_string()), "per-turn override = the default");

    cmd(&k, "/model alpha extra high").await;
    assert_eq!(agent(&k).0, "fake-alpha");
    assert_eq!(agent(&k).1, "xhigh", "\"extra high\" maps to the model's xhigh");
    agent_run::run_prompt(&k, "second", Duration::from_secs(30)).await.unwrap();
    assert_eq!(turn_starts(&codex_log).last().unwrap(), &("fake-alpha".to_string(), "xhigh".to_string()));

    // build mode has its own default
    cmd(&k, "/model build gamma ultra").await;
    k.host_action("mode.set", &[&json!({ "mode": "build" })]).unwrap();
    agent_run::run_prompt(&k, "make a thing", Duration::from_secs(30)).await.unwrap();
    assert_eq!(turn_starts(&codex_log).last().unwrap(), &("fake-gamma".to_string(), "ultra".to_string()), "build default");
    k.host_action("mode.set", &[&json!({ "mode": "chat" })]).unwrap();
    assert_eq!(agent(&k).0, "fake-alpha", "chat mode keeps its own choice");

    // escalation: strong tier, policy auto, a follow-up turn on the strong model, then revert
    cmd(&k, "/model strong gamma").await;
    cmd(&k, "/model policy auto").await;
    let before = turn_starts(&codex_log).len();
    let r = escalating_turn(&k, "SLOW hard task").await;
    assert_eq!(r["tier"], "strong", "{r}");
    let starts = turn_starts(&codex_log);
    assert_eq!(starts.len(), before + 2, "the task turn plus one follow-up: {starts:?}");
    assert_eq!(starts[before], ("fake-alpha".to_string(), "xhigh".to_string()));
    assert_eq!(starts[before + 1], ("fake-gamma".to_string(), "high".to_string()), "follow-up on the strong model (its default effort)");
    assert_eq!(agent(&k).2, "default", "reverted after the task");
    agent_run::run_prompt(&k, "after", Duration::from_secs(30)).await.unwrap();
    assert_eq!(turn_starts(&codex_log).last().unwrap(), &("fake-alpha".to_string(), "xhigh".to_string()), "back on the default");

    // the provider refuses the chosen model → it is dropped and the default takes over
    let _ = agent_run::run_prompt(&k, "REJECT this", Duration::from_secs(30)).await;
    wait_for("the fallback", Duration::from_secs(10), || !catalog_ids(&k, "codex").contains(&"fake-alpha".to_string())).await;
    assert_eq!(agent(&k).0, "fake-beta", "fallback to the harness default");
    assert!(k.snapshot().chat.iter().any(|i| i["text"].as_str().is_some_and(|t| t.contains("cannot use fake-alpha"))), "one system line tells the user");

    // refresh with hidden models; the refused model stays out
    let r = k.ui_action_value("chat", json!({ "host": "models.refresh", "includeHidden": true }), None).await.unwrap();
    assert!(r.to_string().contains("fake-hidden"), "{r}");
    assert_eq!(catalog_ids(&k, "codex"), ["fake-beta", "fake-gamma", "fake-hidden"]);
    let lists = log_lines(&codex_log).into_iter().filter(|v| v["method"] == "model/list").count();
    assert!(lists >= 4, "paginated model/list calls: {lists}");
    let research = std::fs::read_to_string(k.state_dir.join("research.jsonl")).unwrap();
    assert!(research.contains("\"models_discovered\"") && research.contains("\"model_switch\"") && research.contains("fake hard task"), "switches are research-logged");
    k.shutdown();

    // ---------- Pi (fake RPC process through OPENAPP_PI_CMD) ----------
    let pi_log = root.join("pi.jsonl");
    unsafe {
        std::env::set_var("OPENAPP_PI_CMD", format!("node {}", fakes.join("fake_pi.ts").display()));
        std::env::set_var("OPENAPP_FAKE_LOG", &pi_log);
    }
    let k = Kernel::new_opts(root.join("ws-pi"), None, Some("pi"), Arc::new(agent_run::Print::default())).unwrap();
    k.start(true).await.unwrap();
    agent_run::wait_ready(&k, Duration::from_secs(60)).await.unwrap();
    assert_eq!(catalog_ids(&k, "pi"), ["fakeco/fake-pi-one", "fakeco/fake-pi-two"], "Pi discovery, provider-qualified ids");
    let one = k.snapshot().models.into_iter().find(|m| m["id"] == "fakeco/fake-pi-one").unwrap();
    assert_eq!(one["efforts"], json!(["low", "medium", "high", "xhigh"]), "Pi levels ∩ the account's efforts (ultra and max dropped)");
    assert_eq!(pi_calls(&pi_log, "set_model"), ["fakeco/fake-pi-two"], "start: the discovered default replaces Pi's own startup model");
    assert_eq!(pi_calls(&pi_log, "set_thinking_level"), ["medium"]);

    cmd(&k, "/model one xhigh").await;
    agent_run::run_prompt(&k, "hello pi", Duration::from_secs(30)).await.unwrap();
    assert_eq!(pi_calls(&pi_log, "set_model").last().unwrap(), "fakeco/fake-pi-one");
    assert_eq!(pi_calls(&pi_log, "set_thinking_level").last().unwrap(), "xhigh");
    assert!(agent_run::last_agent_text(&k).contains("fakeco/fake-pi-one xhigh"), "the turn ran on the chosen model");

    // escalation in Pi switches mid-turn (no follow-up turn) and reverts on the next turn
    cmd(&k, "/model strong two").await;
    // policy ask (default): a kernel card; "Keep" declines and nothing switches
    let sets = pi_calls(&pi_log, "set_model").len();
    k.send_prompt("SLOW ask first".to_string());
    wait_for("the turn to run", Duration::from_secs(20), || k.running_session().is_some()).await;
    let k2 = k.clone();
    let ask = tokio::spawn(async move { openapp_lib::primitives::call_tool(&k2, "openapp_model_request", json!({ "tier": "strong", "reason": "fake ask" })).await });
    let mut card = None;
    wait_for("the switch card", Duration::from_secs(10), || {
        card = k.snapshot().chat.iter().find(|i| i["kind"] == "permission" && i.get("resolved").is_none() && i["title"].as_str().is_some_and(|t| t.contains("Fake Pi Two"))).and_then(|i| i["id"].as_str().map(String::from));
        card.is_some()
    })
    .await;
    let card = card.unwrap();
    wait_for("the card's waiter", Duration::from_secs(5), || k.respond_permission(&card, Some("keep".into())).is_ok()).await;
    let r = ask.await.unwrap().unwrap().unwrap();
    assert_eq!(r["ok"], false, "declined: {r}");
    wait_for("the task to end", Duration::from_secs(30), || k.running_session().is_none()).await;
    assert_eq!(pi_calls(&pi_log, "set_model").len(), sets, "no switch after Keep");
    cmd(&k, "/model policy auto").await;
    let r = escalating_turn(&k, "SLOW pi task").await;
    assert_eq!(r["tier"], "strong", "{r}");
    assert_eq!(pi_calls(&pi_log, "set_model").last().unwrap(), "fakeco/fake-pi-two", "switched during the turn");
    agent_run::run_prompt(&k, "after pi", Duration::from_secs(30)).await.unwrap();
    assert_eq!(pi_calls(&pi_log, "set_model").last().unwrap(), "fakeco/fake-pi-one", "reverted");
    k.shutdown();

    unsafe {
        std::env::remove_var("OPENAPP_PI_CMD");
        std::env::remove_var("OPENAPP_CODEX_CMD");
    }
    let _ = std::fs::remove_dir_all(&root);

    // ---------- no model id literals ----------
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let hits = literal_hits(&repo);
    assert!(hits.is_empty(), "model id literals found:\n{}", hits.join("\n"));
}
