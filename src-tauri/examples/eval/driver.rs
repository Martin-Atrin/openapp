//! The scripted user both eval runs share: VM-state guard, kernel boot + sign-in check, the card answers
//! (question → scripted answer or first option, secret → cancel, permission → policy), and one build turn
//! (send, answer cards, wait for the turn end or the deadline).

use crate::probe_defs::egress_matches;
use openapp_lib::kernel::{EventSink, Kernel};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Remembers notifications (E9 and the raw transcripts); everything else is read from `snapshot()`.
#[derive(Default)]
pub struct Collect {
    pub notes: Mutex<Vec<Value>>,
}
impl EventSink for Collect {
    fn emit(&self, event: &str, payload: Value) {
        if event == "oa://notify" {
            let mut n = self.notes.lock().unwrap_or_else(|e| e.into_inner());
            if n.len() < 200 {
                n.push(payload);
            }
        }
    }
}

pub fn short(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ").replace('|', "/");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

pub fn read_research(state: &Path) -> Vec<Value> {
    std::fs::read_to_string(state.join("research.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

pub fn turn_ends(state: &Path, sid: &str) -> usize {
    read_research(state).iter().filter(|r| r["kind"] == "turn_end" && r["data"]["session"] == sid).count()
}

/// Refusal phrases in `text` (lowercase), each with some context before it.
pub fn refusal_hits(text: &str, phrases: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for ph in phrases {
        if let Some(at) = text.find(ph) {
            let start = text[..at].char_indices().rev().nth(50).map(|(i, _)| i).unwrap_or(0);
            out.push(format!("\"{ph}\": …{}…", short(&text[start..].chars().take(140).collect::<String>(), 140)));
        }
    }
    out
}

/// `OPENAPP_VM_HOME` = `vm_home`, the app's guest image tar as the default image source, then refuse while a VM
/// of that state looks running: two VMs must never mount the same /opt/openapp volume.
/// Call before the tokio runtime starts (it sets env vars).
pub fn prepare_vm_state(vm_home: &Path, force: bool) {
    let home = dirs::home_dir().unwrap_or_default();
    // SAFETY: single-threaded here, before the runtime starts.
    unsafe {
        std::env::set_var("OPENAPP_VM_HOME", vm_home);
        let app_tar = home.join(".openapp/vm/images").join(format!("openapp-guest-{}.tar", openapp_lib::vm::GUEST_IMAGE_VERSION));
        if std::env::var_os("OPENAPP_GUEST_IMAGE_TAR").is_none() && app_tar.is_file() {
            std::env::set_var("OPENAPP_GUEST_IMAGE_TAR", &app_tar);
        }
    }
    // live owner + accepting socket; stale sockets of a quit or crashed app are removed
    let running = openapp_lib::vm::running_vms(&openapp_lib::vm::vm_home());
    if !running.is_empty() && !force {
        println!("refusing: a VM of {} looks running ({running:?}). Close OpenApp first (or --force for stale sockets).", vm_home.display());
        std::process::exit(2);
    }
}

/// Default VM state for eval runs: a scratch state that holds the harness logins, never the app's own.
pub fn default_vm_home() -> PathBuf {
    std::env::var_os("OPENAPP_VM_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".openapp/vm-bakeoff"))
}

/// Fresh workspace dir (and its state dir) for one run.
pub fn fresh_workspace(ws: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_dir_all(ws);
    std::fs::create_dir_all(ws)?;
    let _ = std::fs::remove_dir_all(openapp_lib::state::state_dir(ws));
    Ok(())
}

/// Start the kernel (VM + harness), wait until the agent is ready, and require a signed-in harness.
/// Ok = the harness card note; Err = why the harness cannot run here (the kernel is shut down).
pub async fn start_signed_in(k: &Arc<Kernel>, harness: &str) -> Result<String, String> {
    if let Err(e) = k.start(true).await {
        return Err(format!("kernel start: {e}"));
    }
    let deadline = Instant::now() + Duration::from_secs(900);
    let ready = loop {
        let (s, e) = k.agent_status();
        if s == "ready" {
            break Ok(());
        }
        if s == "error" || s == "unselected" || Instant::now() > deadline {
            break Err(format!("{s}: {}", e.unwrap_or_default()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let card = k.snapshot().harnesses.into_iter().find(|x| x.id == harness);
    let why = match (ready, card.as_ref().and_then(|c| c.signed_in)) {
        (Err(e), _) => format!("agent did not start: {}", short(&e, 200)),
        (Ok(()), Some(true)) => return Ok(card.and_then(|c| c.note).unwrap_or_default()),
        (Ok(()), s) => format!("{harness} not signed in in this VM state ({}): {}", if s.is_none() { "unknown" } else { "signed out" }, card.and_then(|c| c.note).unwrap_or_default()),
    };
    stop(k).await;
    Err(why)
}

pub async fn stop(k: &Arc<Kernel>) {
    k.shutdown();
    tokio::time::sleep(Duration::from_secs(2)).await;
}

// ---------- the scripted user ----------

/// What the scripted user answers. `None` lists = allow every card of that kind.
pub struct Script {
    /// substrings matched against question option labels (case-insensitive, either direction)
    pub answers: &'static [&'static str],
    /// typed answer when no option matches and the question takes free text (else: first option)
    pub typed: Option<&'static str>,
    /// egress allow patterns (`probe_defs::egress_matches`); also decides secret-to-host cards
    pub egress: Option<&'static [&'static str]>,
    /// consent kinds to allow: host_module | microphone | camera | lan | clipboard | shortcut
    pub consent: Option<&'static [&'static str]>,
}

/// What the scripted user saw and did.
#[derive(Default)]
pub struct Stats {
    pub cards: BTreeMap<String, u32>,
    pub decisions: Vec<String>,
    pub questions: Vec<String>,
    pub fallbacks: Vec<String>,
    pub secrets: Vec<String>,
    pub permissions: Vec<String>,
    pub egress_allowed: Vec<String>,
    pub egress_denied: Vec<String>,
    pub consent_denied: Vec<String>,
    pub harness_cards: Vec<String>,
    pub followups: Vec<String>,
    pub needs_human: Vec<String>,
    pub timed_out: bool,
    pub turn_errors: Vec<String>,
}

impl Stats {
    pub fn to_json(&self) -> Value {
        json!({
            "cards": self.cards, "decisions": self.decisions, "questions": self.questions, "fallbacks": self.fallbacks,
            "secrets": self.secrets, "permissions": self.permissions, "egress_allowed": self.egress_allowed, "egress_denied": self.egress_denied,
            "consent_denied": self.consent_denied, "harness_cards": self.harness_cards, "followups": self.followups,
            "needs_human": self.needs_human, "turn_errors": self.turn_errors, "timed_out": self.timed_out,
        })
    }
}

fn is_open(i: &Value) -> bool {
    i.get("resolved").is_none_or(Value::is_null)
}

/// Card kind from the chat item (kernel cards have fixed title shapes, everything else is a harness approval).
fn classify(i: &Value) -> (&'static str, String) {
    let title = i["title"].as_str().unwrap_or("");
    let t = title.to_lowercase();
    if let Some(at) = t.find("wants to reach ") {
        return ("egress", title[at + "wants to reach ".len()..].trim().to_string());
    }
    if t.contains("wants to run code on your mac") {
        return ("host_module", i["details"]["module"].as_str().unwrap_or("").to_string());
    }
    for (needle, kind) in [("microphone", "microphone"), ("use the camera", "camera"), ("local network", "lan"), ("read the clipboard", "clipboard"), ("global shortcut", "shortcut"), ("in the web view", "web_control")] {
        if t.contains(needle) {
            return (kind, String::new());
        }
    }
    if t.contains("to your keychain") && t.starts_with("save ") {
        return ("oauth_save", String::new());
    }
    if t.contains("from your keychain") {
        return ("secret_delete", String::new());
    }
    if t.contains("wants to send \"") {
        let host = t.rsplit(" to ").next().unwrap_or("").trim().to_string();
        return ("secret_host", host);
    }
    ("harness", String::new())
}

fn pick_option(i: &Value, allow: bool, prefer: &[&str]) -> Option<String> {
    let opts = i["options"].as_array()?;
    let id = |o: &Value| o["optionId"].as_str().map(String::from);
    if allow {
        for p in prefer {
            if let Some(o) = opts.iter().find(|o| o["optionId"] == *p || o["kind"] == *p) {
                return id(o);
            }
        }
        opts.iter().find(|o| o["kind"].as_str().is_some_and(|k| k.starts_with("allow"))).or(opts.first()).and_then(id)
    } else {
        opts.iter().find(|o| o["kind"].as_str().is_some_and(|k| k.starts_with("reject")) || o["optionId"] == "deny").and_then(id)
    }
}

impl Script {
    fn answers_for(&self, q: &Value, st: &mut Stats) -> Map<String, Value> {
        let mut m = Map::new();
        for qq in q["questions"].as_array().cloned().unwrap_or_default() {
            let Some(id) = qq["id"].as_str() else { continue };
            let text = short(qq["question"].as_str().unwrap_or(""), 140);
            let labels: Vec<String> = qq["options"].as_array().map(|a| a.iter().filter_map(|o| o["label"].as_str().map(String::from)).collect()).unwrap_or_default();
            let matched = self.answers.iter().find_map(|a| {
                let al = a.to_lowercase();
                labels.iter().find(|l| {
                    let ll = l.to_lowercase();
                    ll.contains(&al) || (!ll.is_empty() && al.contains(&ll))
                })
            });
            let free_text = qq["isOther"] != false || labels.is_empty();
            let (pick, how) = match (matched, self.typed.filter(|_| free_text), labels.first()) {
                (Some(l), _, _) => (l.clone(), "matched"),
                (None, Some(t), _) => (t.to_string(), "typed"),
                (None, None, Some(l)) => {
                    st.fallbacks.push(format!("{text} -> first option \"{l}\""));
                    (l.clone(), "fallback first option")
                }
                (None, None, None) => {
                    st.fallbacks.push(format!("{text} -> free text"));
                    ("Use your best judgment; keep it simple.".to_string(), "free text")
                }
            };
            st.questions.push(format!("{text} -> {pick} ({how})"));
            m.insert(id.into(), json!([pick]));
        }
        m
    }

    fn egress_ok(&self, target: &str) -> bool {
        self.egress.is_none_or(|l| l.iter().any(|pat| egress_matches(pat, target)))
    }

    /// Answer every open card once.
    pub fn handle_cards(&self, k: &Arc<Kernel>, st: &mut Stats, handled: &mut HashSet<String>) {
        for i in k.snapshot().chat.iter().filter(|i| is_open(i)) {
            let Some(id) = i["id"].as_str().map(String::from) else { continue };
            if handled.contains(&id) {
                continue;
            }
            match i["kind"].as_str().unwrap_or("") {
                "question" => {
                    *st.cards.entry("question".into()).or_default() += 1;
                    if i["questions"].as_array().is_some_and(|a| a.iter().any(|q| q["isSecret"] == true)) {
                        st.secrets.push(format!("secret question: {}", short(&i["questions"].to_string(), 100)));
                        let _ = k.answer_question(&id, None);
                    } else {
                        let a = self.answers_for(i, st);
                        let _ = k.answer_question(&id, Some(a));
                    }
                    handled.insert(id);
                }
                "secret" => {
                    *st.cards.entry("secret".into()).or_default() += 1;
                    st.secrets.push(format!("{} ({}){}", i["name"].as_str().unwrap_or(""), i["secretKind"].as_str().unwrap_or(""), i["hosts"].as_array().map(|h| format!(" hosts {h:?}")).unwrap_or_default()));
                    let _ = k.submit_secret(&id, None);
                    handled.insert(id);
                }
                "permission" => {
                    let (kind, target) = classify(i);
                    *st.cards.entry(kind.into()).or_default() += 1;
                    let title = short(i["title"].as_str().unwrap_or(""), 120);
                    let (allow, prefer): (bool, &[&str]) = match kind {
                        "egress" => {
                            let ok = self.egress_ok(&target);
                            if ok { st.egress_allowed.push(target.clone()) } else { st.egress_denied.push(target.clone()) }
                            st.decisions.push(format!("egress {target} -> {}", if ok { "allow" } else { "deny" }));
                            (ok, &["allow_workspace", "allow_once"])
                        }
                        "host_module" | "microphone" | "camera" | "lan" | "clipboard" | "shortcut" => {
                            let ok = self.consent.is_none_or(|c| c.contains(&kind));
                            let extra = if kind == "host_module" {
                                let d = &i["details"];
                                format!(" [module {} imports {} calls {}]", d["module"], d["imports"], d["calls"])
                            } else {
                                String::new()
                            };
                            if !ok {
                                st.consent_denied.push(format!("{kind}: {title}"));
                            }
                            st.decisions.push(format!("{kind}: {title}{extra} -> {}", if ok { "allow" } else { "deny" }));
                            (ok, &["allow", "allow_always"])
                        }
                        "secret_host" => {
                            let ok = self.egress_ok(&target);
                            st.decisions.push(format!("secret_host: {title} -> {}", if ok { "allow" } else { "deny" }));
                            (ok, &["allow"])
                        }
                        "web_control" | "oauth_save" | "secret_delete" => {
                            st.consent_denied.push(format!("{kind}: {title}"));
                            (false, &[])
                        }
                        _ => {
                            // the harness' own approvals (commands, file changes): allow once, record
                            st.harness_cards.push(title.clone());
                            (true, &["allow_once"])
                        }
                    };
                    st.permissions.push(format!("{title} -> {}", if allow { "allow" } else { "deny" }));
                    let _ = k.respond_permission(&id, pick_option(i, allow, prefer));
                    handled.insert(id);
                }
                _ => {}
            }
        }
    }

    /// One turn: send `prompt` in session `sid`, answer cards until the turn ends (research `turn_end`, or 15 s
    /// idle after working, or an early error) or `deadline` passes (then the turn is cancelled and
    /// `st.timed_out` set). Returns false on timeout.
    pub async fn turn(&self, k: &Arc<Kernel>, sid: &str, prompt: &str, deadline: Instant, st: &mut Stats, handled: &mut HashSet<String>) -> bool {
        let baseline = turn_ends(&k.state_dir, sid);
        let t0 = Instant::now();
        k.send_prompt(prompt.to_string());
        let mut seen_working = false;
        let mut idle_since: Option<Instant> = None;
        let mut last_poll = Instant::now() - Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            self.handle_cards(k, st, handled);
            if last_poll.elapsed() > Duration::from_millis(700) {
                last_poll = Instant::now();
                if turn_ends(&k.state_dir, sid) > baseline {
                    return true;
                }
            }
            let (s, _) = k.agent_status();
            if s == "working" {
                seen_working = true;
                idle_since = None;
            } else if seen_working && idle_since.is_none() {
                idle_since = Some(Instant::now());
            }
            if idle_since.is_some_and(|t| t.elapsed() > Duration::from_secs(15)) {
                return true;
            }
            if !seen_working && t0.elapsed() > Duration::from_secs(60) && k.snapshot().chat.last().is_some_and(|i| i["kind"] == "error") {
                st.turn_errors.push(short(k.snapshot().chat.last().and_then(|i| i["text"].as_str()).unwrap_or(""), 200));
                return true;
            }
            if Instant::now() > deadline {
                st.timed_out = true;
                k.cancel_turn();
                tokio::time::sleep(Duration::from_secs(5)).await;
                return false;
            }
        }
    }

    /// Keep answering cards for `secs` (cards that follow a turn: module loads, services).
    pub async fn settle(&self, k: &Arc<Kernel>, secs: f32, st: &mut Stats, handled: &mut HashSet<String>) {
        for _ in 0..(secs * 4.0) as usize {
            tokio::time::sleep(Duration::from_millis(250)).await;
            self.handle_cards(k, st, handled);
        }
    }
}

/// Agent texts of the chat (non-empty), oldest first.
pub fn agent_texts(chat: &[Value]) -> Vec<String> {
    chat.iter().filter(|i| i["kind"] == "agent").filter_map(|i| i["text"].as_str().map(String::from)).filter(|t| !t.trim().is_empty()).collect()
}

/// `(all tool calls, openapp MCP tool calls)`.
pub fn tool_counts(chat: &[Value]) -> (usize, usize) {
    let tools: Vec<&Value> = chat.iter().filter(|i| i["kind"] == "tool").collect();
    (tools.len(), tools.iter().filter(|i| i["title"].as_str().is_some_and(|t| t.starts_with("openapp"))).count())
}

pub fn errors_of(chat: &[Value]) -> Vec<String> {
    chat.iter().filter(|i| i["kind"] == "error").map(|i| short(i["text"].as_str().unwrap_or(""), 200)).collect()
}

pub fn list(v: &[String], max: usize) -> String {
    if v.is_empty() {
        "—".into()
    } else {
        v.iter().take(max).map(|s| format!("`{}`", short(s, 150))).collect::<Vec<_>>().join(", ") + if v.len() > max { " …" } else { "" }
    }
}

/// Output dir under the repo's `eval/` folder.
pub fn eval_dir(sub: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../eval")).join(sub)
}
