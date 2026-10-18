//! `eval bakeoff`: Codex vs Pi (contract §11, §12, §17, §19.2, §20). For each harness: a fresh scratch
//! workspace, its own app VM, and three build-mode prompts, each in a fresh session. The scripted user
//! (driver.rs) answers cards (question → scripted answer, secret → cancel, permission → allow) and the run
//! records per prompt: refusal phrases, questions, secret requests, modules created, files changed, shell
//! commands, tool calls, turn time and the final summary. Harnesses that are not signed in inside the VM
//! state are skipped and reported.
//!
//! Env:  OPENAPP_VM_HOME — VM state with the logins (default ~/.openapp/vm-bakeoff, a scratch state).
//!         The app's own state (~/.openapp/vm) is used only when set explicitly, and only while the
//!         app is closed: two VMs must never mount the same /opt/openapp volume at once.
//!       OPENAPP_BAKEOFF_HARNESSES (default "codex,pi"), OPENAPP_BAKEOFF_TURN_S (default 900),
//!       OPENAPP_BAKEOFF_WS (default <tmp>/openapp-bakeoff), OPENAPP_GUEST_IMAGE_TAR (default: the app's),
//!       OPENAPP_BAKEOFF_FORCE=1 (same as --force: ignore stale VM sockets).
//! Out:  eval/bakeoff/RESULTS-<date>.md + eval/bakeoff/transcripts-<date>/<harness>-<n>.json

use crate::driver::{self, Collect, Script, Stats, list, short};
use openapp_lib::kernel::Kernel;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const USAGE: &str = "usage: eval bakeoff [--force]   (env: OPENAPP_VM_HOME, OPENAPP_BAKEOFF_HARNESSES, OPENAPP_BAKEOFF_TURN_S, OPENAPP_BAKEOFF_WS)";

struct Prompt {
    text: &'static str,
    expect: &'static str,
    /// scripted answers for question cards (option label match, else typed)
    answers: &'static [&'static str],
    typed: Option<&'static str>,
}

const PROMPTS: &[Prompt] = &[
    Prompt {
        text: "connect my Gmail",
        expect: "question card → answered \"IMAP with an app password\" → secret.request card (cancelled) → explains the next step, no refusal",
        answers: &["IMAP", "app password"],
        typed: Some("IMAP with an app password"),
    },
    Prompt {
        text: "build an agent that triages my inbox every morning and shows a summary card",
        expect: "connector may be missing → builds an agent module skeleton and asks/plans, no refusal",
        answers: &[],
        typed: None,
    },
    Prompt {
        text: "add a voice dictation button next to Send",
        expect: "audio.record with transcribe \"apple\" and insert (host speech service), no Whisper",
        answers: &[],
        typed: None,
    },
];

const REFUSALS: &[&str] = &[
    "i can't",
    "i cannot",
    "i can not",
    "i'm unable",
    "i am unable",
    "unable to",
    "not supported",
    "isn't supported",
    "is not supported",
    "don't have",
    "do not have",
    "not possible",
    "no way to",
];

#[derive(Default)]
struct Outcome {
    harness: String,
    prompt: usize,
    status: String,
    ms: u128,
    stats: Stats,
    refusals: Vec<String>,
    modules: Vec<String>,
    files: Vec<String>,
    commands: Vec<String>,
    tools: usize,
    mcp_tools: usize,
    summary: String,
    checks: Vec<(String, bool)>,
    errors: Vec<String>,
}

/// rel path → (size, mtime) of the user's app (no .git / .openapp / node_modules / agent configs).
fn files(ws: &Path) -> BTreeMap<String, (u64, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![ws.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".git" | ".openapp" | "node_modules" | ".claude" | ".pi" | "__pycache__") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(ws) {
                out.insert(rel.to_string_lossy().to_string(), (md.len(), md.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    out
}

async fn run_prompt(k: &Arc<Kernel>, harness: &str, n: usize, p: &Prompt, turn_limit: Duration) -> (Outcome, Vec<Value>) {
    let mut o = Outcome { harness: harness.into(), prompt: n + 1, ..Default::default() };
    let user = Script { answers: p.answers, typed: p.typed, egress: None, consent: None };
    let sid = k.session_new();
    let before_files = files(&k.workspace);
    let before_mods: BTreeSet<String> = openapp_lib::workspace::module_dirs(&k.workspace).into_iter().collect();
    let t0 = Instant::now();
    let mut handled = HashSet::new();
    let ended = user.turn(k, &sid, &format!("/build {}", p.text), t0 + turn_limit, &mut o.stats, &mut handled).await;
    o.ms = t0.elapsed().as_millis();
    o.status = if ended { "ended".into() } else { format!("timeout after {} s (cancelled)", turn_limit.as_secs()) };
    let chat = k.snapshot().chat;
    let agent = driver::agent_texts(&chat);
    o.summary = agent.last().cloned().unwrap_or_default();
    let all = agent.join("\n").to_lowercase();
    o.refusals = driver::refusal_hits(&all, REFUSALS);
    (o.tools, o.mcp_tools) = driver::tool_counts(&chat);
    o.commands = chat.iter().filter(|i| i["kind"] == "tool").filter_map(|i| i["command"].as_str()).filter(|c| !c.is_empty()).map(|c| short(c, 100)).collect();
    o.errors = driver::errors_of(&chat);
    let after = files(&k.workspace);
    for (f, meta) in &after {
        match before_files.get(f) {
            None => o.files.push(format!("+{f}")),
            Some(m) if m != meta => o.files.push(format!("~{f}")),
            _ => {}
        }
    }
    for f in before_files.keys().filter(|f| !after.contains_key(*f)) {
        o.files.push(format!("-{f}"));
    }
    o.modules = openapp_lib::workspace::module_dirs(&k.workspace).into_iter().filter(|m| !before_mods.contains(m)).collect();
    // expectations
    let changed_text: String = o
        .files
        .iter()
        .filter_map(|f| f.get(1..))
        .filter_map(|f| std::fs::read_to_string(k.workspace.join(f)).ok())
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let no_refusal = o.refusals.is_empty();
    let asked = o.stats.cards.get("question").copied().unwrap_or(0) > 0;
    let secrets = o.stats.cards.get("secret").copied().unwrap_or(0) > 0 || o.stats.secrets.iter().any(|s| s.starts_with("secret question"));
    match n {
        0 => {
            o.checks.push(("asked a question".into(), asked));
            o.checks.push(("requested a secret (secret.request)".into(), secrets));
            o.checks.push(("explained the next step (final text)".into(), o.summary.trim().len() > 20));
            o.checks.push(("no refusal phrases".into(), no_refusal));
        }
        1 => {
            o.checks.push(("created a module / files".into(), !o.modules.is_empty() || o.files.iter().any(|f| f.contains("modules/"))));
            o.checks.push(("asked or planned".into(), asked || o.summary.trim().len() > 20));
            o.checks.push(("no refusal phrases".into(), no_refusal));
        }
        _ => {
            let blob = format!("{changed_text}\n{all}");
            o.checks.push(("uses audio.record".into(), blob.contains("audio.record")));
            o.checks.push(("transcribe via apple (host speech)".into(), blob.contains("transcribe") && blob.contains("apple")));
            o.checks.push(("inserts into the composer".into(), blob.contains("insert")));
            o.checks.push(("no Whisper".into(), !blob.contains("whisper") && !o.commands.iter().any(|c| c.to_lowercase().contains("whisper"))));
            o.checks.push(("no refusal phrases".into(), no_refusal));
        }
    }
    (o, chat)
}

pub fn main(args: Vec<String>) -> anyhow::Result<()> {
    let mut force = std::env::var("OPENAPP_BAKEOFF_FORCE").is_ok_and(|v| v == "1");
    for a in &args {
        match a.as_str() {
            "--force" => force = true,
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            other => {
                eprintln!("unknown argument {other}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    driver::prepare_vm_state(&driver::default_vm_home(), force);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let harnesses: Vec<String> = std::env::var("OPENAPP_BAKEOFF_HARNESSES").unwrap_or_else(|_| "codex,pi".into()).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let turn_limit = Duration::from_secs(std::env::var("OPENAPP_BAKEOFF_TURN_S").ok().and_then(|v| v.parse().ok()).unwrap_or(900));
    let root: PathBuf = std::env::var_os("OPENAPP_BAKEOFF_WS").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("openapp-bakeoff"));
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let out_dir = driver::eval_dir("bakeoff");
    let tr_dir = out_dir.join(format!("transcripts-{date}"));
    println!("vm home: {}  workspaces: {}  harnesses: {harnesses:?}", openapp_lib::vm::vm_home().display(), root.display());
    let mut results: Vec<Outcome> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut meta: Vec<(String, String)> = Vec::new();
    for h in &harnesses {
        let ws = root.join(h);
        driver::fresh_workspace(&ws)?;
        println!("== {h}: workspace {}", ws.display());
        let k = Kernel::new_opts(ws.clone(), None, Some(h.as_str()), Arc::new(Collect::default()))?;
        match driver::start_signed_in(&k, h).await {
            Err(why) => {
                skipped.push((h.clone(), why));
                continue;
            }
            Ok(note) => {
                let cfg = driver::read_research(&k.state_dir).into_iter().rev().find(|r| r["kind"] == "harness_config").map(|r| r["data"].clone()).unwrap_or_default();
                meta.push((h.clone(), format!("{note} · {}", short(&cfg.to_string(), 200))));
            }
        }
        for (n, p) in PROMPTS.iter().enumerate() {
            println!("-- {h} #{}: {}", n + 1, p.text);
            let (o, chat) = run_prompt(&k, h, n, p, turn_limit).await;
            println!("   {} in {} s, {} questions, {} secrets, {} tools, {} files, checks {:?}", o.status, o.ms / 1000, o.stats.questions.len(), o.stats.secrets.len(), o.tools, o.files.len(), o.checks);
            let _ = std::fs::create_dir_all(&tr_dir);
            let _ = std::fs::write(tr_dir.join(format!("{h}-{}.json", n + 1)), serde_json::to_string_pretty(&chat).unwrap_or_default());
            results.push(o);
        }
        driver::stop(&k).await;
    }

    // report
    let mut md = format!(
        "# Codex vs Pi bake-off — {date}\n\nRunner: `src-tauri/examples/eval.rs bakeoff` (build mode, fresh workspace per harness, fresh session per prompt; questions auto-answered, secret requests cancelled, permission cards allowed). VM state: `{}`. Turn limit: {} s.\n\n",
        openapp_lib::vm::vm_home().display(),
        turn_limit.as_secs()
    );
    md.push_str("## Prompts\n\n");
    for (n, p) in PROMPTS.iter().enumerate() {
        md.push_str(&format!("{}. **{}** — expect: {}\n", n + 1, p.text, p.expect));
    }
    if !meta.is_empty() {
        md.push_str("\n## Harnesses\n\n");
        for (h, m) in &meta {
            md.push_str(&format!("- `{h}`: {m}\n"));
        }
    }
    if !skipped.is_empty() {
        md.push_str("\n## Skipped\n\n");
        for (h, why) in &skipped {
            md.push_str(&format!("- `{h}`: {why}\n"));
        }
    }
    if !results.is_empty() {
        md.push_str("\n## Summary\n\n| Harness | # | Status | Time (s) | Refusal? | Questions | Secrets | Modules | Files | Shell cmds | Tools (MCP) | Checks |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for o in &results {
            let passed = o.checks.iter().filter(|(_, ok)| *ok).count();
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} ({}) | {}/{} |\n",
                o.harness,
                o.prompt,
                o.status,
                o.ms / 1000,
                if o.refusals.is_empty() { "no" } else { "yes" },
                o.stats.questions.len(),
                o.stats.secrets.len(),
                o.modules.len(),
                o.files.len(),
                o.commands.len(),
                o.tools,
                o.mcp_tools,
                passed,
                o.checks.len()
            ));
        }
        md.push_str("\n## Details\n");
        for o in &results {
            md.push_str(&format!("\n### {} · prompt {}\n\n", o.harness, o.prompt));
            for (c, ok) in &o.checks {
                md.push_str(&format!("- [{}] {c}\n", if *ok { "x" } else { " " }));
            }
            md.push_str(&format!("- Refusal phrases: {}\n", list(&o.refusals, 5)));
            md.push_str(&format!("- Questions: {}\n", list(&o.stats.questions, 5)));
            md.push_str(&format!("- Secret requests: {}\n", list(&o.stats.secrets, 5)));
            md.push_str(&format!("- Permission cards: {}\n", list(&o.stats.permissions, 8)));
            md.push_str(&format!("- Modules created: {}\n", list(&o.modules, 10)));
            md.push_str(&format!("- Files changed ({}): {}\n", o.files.len(), list(&o.files, 15)));
            md.push_str(&format!("- Shell commands ({}): {}\n", o.commands.len(), list(&o.commands, 12)));
            md.push_str(&format!("- Errors: {}\n", list(&o.errors, 5)));
            md.push_str(&format!("- Final summary:\n\n  > {}\n", short(&o.summary, 1200)));
        }
        md.push_str(&format!("\nTranscripts: `eval/bakeoff/transcripts-{date}/`.\n"));
    } else {
        md.push_str("\nNo harness was signed in, so no prompt ran. Sign in inside this VM state and run again.\n");
    }
    std::fs::create_dir_all(&out_dir)?;
    let path = out_dir.join(format!("RESULTS-{date}.md"));
    std::fs::write(&path, md)?;
    println!("report: {}", path.display());
    for (h, why) in &skipped {
        println!("skipped {h}: {why}");
    }
    Ok(())
}
