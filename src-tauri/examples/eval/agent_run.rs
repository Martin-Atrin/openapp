//! Shared helpers for the real-harness smoke examples.
#![allow(dead_code)]

use openapp_lib::kernel::{EventSink, Kernel};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Prints chat appends and agent status changes; remembers notify/composer events.
#[derive(Default)]
pub struct Print {
    pub events: Mutex<Vec<(String, Value)>>,
    last_status: Mutex<String>,
}

impl EventSink for Print {
    fn emit(&self, event: &str, payload: Value) {
        match event {
            "oa://chat" if payload["op"] == "append" => {
                let i = &payload["item"];
                println!(
                    "  [chat+] {} {} {}",
                    i["kind"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or(""),
                    i["text"].as_str().unwrap_or("").chars().take(160).collect::<String>().replace('\n', " ")
                );
            }
            "oa://snapshot" => {
                let s = format!("{}", payload["agent"]["status"]);
                let mut last = self.last_status.lock().unwrap();
                if *last != s {
                    println!("  [agent] {} {}", payload["agent"]["name"], s);
                    *last = s;
                }
            }
            _ => {}
        }
        self.events.lock().unwrap().push((event.to_string(), payload));
    }
}

pub async fn wait_ready(k: &Kernel, limit: Duration) -> Result<Duration, String> {
    let t0 = Instant::now();
    loop {
        let (s, e) = k.agent_status();
        match s.as_str() {
            "ready" => return Ok(t0.elapsed()),
            "error" | "unselected" => return Err(format!("{s}: {}", e.unwrap_or_default())),
            _ => {}
        }
        if t0.elapsed() > limit {
            return Err("agent did not become ready".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Send a prompt, auto-approve permission cards (first allow option), wait for the turn end.
pub async fn run_prompt(k: &Arc<Kernel>, prompt: &str, limit: Duration) -> Result<Duration, String> {
    k.send_prompt(prompt.to_string());
    let t0 = Instant::now();
    let mut seen_working = false;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let open: Vec<(String, String, Option<String>)> = k
            .snapshot()
            .chat
            .iter()
            .filter(|i| i["kind"] == "permission" && i.get("resolved").is_none())
            .map(|i| {
                let opt = i["options"].as_array().and_then(|o| {
                    o.iter().find(|x| x["kind"].as_str().is_some_and(|k| k.starts_with("allow"))).or(o.first())
                });
                (
                    i["id"].as_str().unwrap_or("").to_string(),
                    i["title"].as_str().unwrap_or("").to_string(),
                    opt.and_then(|o| o["optionId"].as_str().map(String::from)),
                )
            })
            .collect();
        for (id, title, opt) in open {
            if k.respond_permission(&id, opt.clone()).is_ok() {
                println!("  [permission auto-approved] {title} -> {opt:?}");
            }
        }
        let (s, _) = k.agent_status();
        if s == "working" {
            seen_working = true;
        }
        if seen_working && s != "working" {
            return Ok(t0.elapsed());
        }
        if !seen_working && t0.elapsed() > Duration::from_secs(5) && s != "starting" {
            // the turn may have failed before "working" was visible
            let last = k.snapshot().chat.last().cloned().unwrap_or(Value::Null);
            if last["kind"] == "error" {
                return Err(last["text"].as_str().unwrap_or("").to_string());
            }
        }
        if t0.elapsed() > limit {
            return Err(format!("turn timed out after {limit:?}"));
        }
    }
}

/// Last agent text in the active chat.
pub fn last_agent_text(k: &Kernel) -> String {
    k.snapshot().chat.iter().rev().find(|i| i["kind"] == "agent").and_then(|i| i["text"].as_str().map(String::from)).unwrap_or_default()
}
