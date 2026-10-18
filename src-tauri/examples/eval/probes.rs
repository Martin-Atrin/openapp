//! `eval probes`: the buildability probes (eval/probes/PROBES.md, E1–E12).
//!
//! For each probe: a fresh scratch workspace, its own kernel + app VM, a fresh session, `/build <prompt>`.
//! The scripted user (driver.rs) answers cards as PROBES.md lists (question → listed answers, egress → allow
//! listed hosts, consent → allow listed kinds, secret → cancel), sends up to two "Continue building"
//! follow-ups, then checks the pass end state from `describe` / `snapshot` JSON and the module sources.
//! Out: eval/probes/LOOP-<n>.md and eval/probes/raw/<n>/<id>.json

use crate::driver::{self, Collect, Script, Stats, list, short};
use crate::probe_defs::*;
use openapp_lib::kernel::Kernel;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const USAGE: &str = "usage: eval probes --harness codex|pi [--only E1,E3] [--timeout-min 20] [--vm-home <dir>] [--dry-run] [--force] [--keep-ws] [--real-folders] [--ws-root <dir>] [--followups 2]";

const FOLLOWUP: &str = "Continue building; I'll do the human-only steps later.";

const REFUSALS: &[&str] = &[
    "can't",
    "cannot",
    "can not",
    "not supported",
    "isn't supported",
    "is not supported",
    "don't have",
    "do not have",
    "unable to",
    "isn't possible",
    "is not possible",
    "not possible",
    "no way to",
    "doesn't support",
    "won't be able",
];

// ---------- options ----------

struct Opts {
    harness: String,
    only: Vec<String>,
    timeout: Duration,
    vm_home: Option<PathBuf>,
    dry: bool,
    force: bool,
    keep_ws: bool,
    real_folders: bool,
    ws_root: PathBuf,
    followups: usize,
}

fn parse_args(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts {
        harness: "codex".into(),
        only: vec![],
        timeout: Duration::from_secs(20 * 60),
        vm_home: None,
        dry: false,
        force: false,
        keep_ws: false,
        real_folders: false,
        ws_root: std::env::var_os("OPENAPP_PROBES_WS").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("openapp-probes")),
        followups: 2,
    };
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = match args[i].split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (args[i].clone(), None),
        };
        let val = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("{key} needs a value"))
        };
        match key.as_str() {
            "--harness" => o.harness = val(&mut i)?,
            "--only" => o.only = val(&mut i)?.split(',').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect(),
            "--timeout-min" => o.timeout = Duration::from_secs((val(&mut i)?.parse::<f64>().map_err(|e| e.to_string())? * 60.0) as u64),
            "--vm-home" => o.vm_home = Some(PathBuf::from(val(&mut i)?)),
            "--ws-root" => o.ws_root = PathBuf::from(val(&mut i)?),
            "--followups" => o.followups = val(&mut i)?.parse().map_err(|e: std::num::ParseIntError| e.to_string())?,
            "--dry-run" => o.dry = true,
            "--force" => o.force = true,
            "--keep-ws" => o.keep_ws = true,
            "--real-folders" => o.real_folders = true,
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    if !matches!(o.harness.as_str(), "codex" | "pi") {
        return Err(format!("--harness must be codex or pi, got {}", o.harness));
    }
    for id in &o.only {
        if !PROBES.iter().any(|p| p.id == id) {
            return Err(format!("unknown probe {id} (E1..E12)"));
        }
    }
    Ok(o)
}

fn out_dir() -> PathBuf {
    driver::eval_dir("probes")
}

fn next_loop() -> u32 {
    let max = std::fs::read_dir(out_dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.strip_prefix("LOOP-")?.strip_suffix(".md")?.parse::<u32>().ok()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    max + 1
}

fn selected(o: &Opts) -> Vec<&'static Probe> {
    PROBES.iter().filter(|p| o.only.is_empty() || o.only.iter().any(|x| x == p.id)).collect()
}

fn vm_home_of(o: &Opts) -> PathBuf {
    o.vm_home.clone().unwrap_or_else(driver::default_vm_home)
}

fn folder_path(o: &Opts, f: &Folder) -> PathBuf {
    if o.real_folders { dirs::home_dir().unwrap_or_default().join(f.home_dir) } else { o.ws_root.join("fixtures").join(f.home_dir) }
}

fn print_plan(o: &Opts, n: u32) {
    let ps = selected(o);
    println!("probe runner plan (dry run, nothing started)");
    println!("  harness: {}  probes: {}  per-probe timeout: {} min  follow-ups: {}", o.harness, ps.iter().map(|p| p.id).collect::<Vec<_>>().join(","), o.timeout.as_secs() / 60, o.followups);
    println!("  vm home: {}  workspaces: {}  keep workspaces: {}", vm_home_of(o).display(), o.ws_root.display(), o.keep_ws);
    println!("  output: {}/LOOP-{n}.md  raw: {}/raw/{n}/<id>.json", out_dir().display(), out_dir().display());
    for p in ps {
        println!("-- {} {}", p.id, p.title);
        println!("   send:     /build {}", p.prompt);
        println!("   answers:  {:?} (fallback: first option, recorded)", p.answers);
        println!("   egress:   allow {:?}; deny + record the rest", p.egress);
        println!("   consent:  allow {:?}; deny + record the rest; secret cards: cancel + record", p.consent);
        for f in p.folders {
            println!("   folder:   {} -> /mnt/{} pre-granted from {} ({})", f.home_dir, f.name, folder_path(o, f).display(), if o.real_folders { "real folder" } else { "fixture copy" });
        }
        println!("   pass:     {}", p.end_state);
        println!("   human:    {}", p.human);
        println!("   checks:   {}", (p.checks)(&Facts::default()).iter().map(|c| format!("{}{}", c.name, match c.kind { Kind::Req => "", Kind::Opt => " (optional)", Kind::Manual => " (manual)" })).collect::<Vec<_>>().join("; "));
    }
}

pub fn main(args: Vec<String>) -> anyhow::Result<()> {
    let o = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let n = next_loop();
    if o.dry {
        print_plan(&o, n);
        return Ok(());
    }
    driver::prepare_vm_state(&vm_home_of(&o), o.force);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(o, n))
}

// ---------- run state ----------

struct Res {
    id: &'static str,
    status: &'static str,
    reason: String,
    ms: u128,
    turns: usize,
    modules: Vec<String>,
    services: Vec<String>,
    stats: Stats,
    checks: Vec<Check>,
    commands: Vec<Cmd>,
    refusals: Vec<String>,
    gaps_named: Vec<String>,
    summary: String,
    tools: usize,
    mcp_tools: usize,
    errors: Vec<String>,
    module_status: Vec<(String, String)>,
    suggestions: Vec<String>,
    skipped: Option<String>,
}

struct Cmd {
    cmd: String,
    cwd: String,
    exit: Option<i64>,
    outside: Vec<String>,
    out_tail: String,
}

// ---------- facts ----------

const TEXT_EXT: &[&str] = &["json", "js", "mjs", "cjs", "ts", "tsx", "jsx", "html", "css", "md", "py", "swift", "sh", "toml", "txt", "yaml", "yml", "rs", "go", "plist", "xml"];

fn module_files(ws: &Path, id: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut total = 0usize;
    let root = ws.join("modules").join(id);
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), "node_modules" | ".git" | "data" | "__pycache__" | "target" | ".venv" | "dist" | "build" | ".build") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else if md.len() <= 256 * 1024
                && e.path().extension().and_then(|x| x.to_str()).is_some_and(|x| TEXT_EXT.contains(&x))
                && total < 4_000_000
                && let Ok(t) = std::fs::read_to_string(e.path())
            {
                total += t.len();
                let rel = e.path().strip_prefix(ws).map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
                out.push((rel, t.to_lowercase()));
            }
        }
    }
    out.sort();
    out
}

fn collect(k: &Arc<Kernel>) -> Facts {
    let mut f = Facts { desc: k.describe(), ..Default::default() };
    let snap = k.snapshot();
    f.snap = serde_json::to_value(&snap).unwrap_or(Value::Null);
    f.chat = snap.chat.clone();
    f.research = driver::read_research(&k.state_dir);
    for id in openapp_lib::workspace::module_dirs(&k.workspace) {
        if let Some(m) = std::fs::read_to_string(k.workspace.join("modules").join(&id).join("openapp.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            f.manifests.push((id.clone(), m));
        }
        f.files.extend(module_files(&k.workspace, &id));
    }
    f.blob = f.files.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
    f.agent_text = f.chat.iter().filter(|i| i["kind"] == "agent").filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n").to_lowercase();
    f.tools_text = f
        .chat
        .iter()
        .filter(|i| i["kind"] == "tool")
        .map(|i| format!("{} {}", i["title"].as_str().unwrap_or(""), i["command"].as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    for r in &f.research {
        if r["kind"] == "host_action"
            && let Some(a) = r["data"]["action"].as_str()
        {
            f.host_actions.insert(a.to_string());
        }
    }
    f.secret_cards = f.chat.iter().filter(|i| i["kind"] == "secret").cloned().collect();
    f.question_cards = f.chat.iter().filter(|i| i["kind"] == "question").cloned().collect();
    f.granted_folders = f.snap["grants"].as_array().map(|a| a.iter().filter(|g| g["kind"] == "folder").filter_map(|g| g["id"].as_str().map(String::from)).collect()).unwrap_or_default();
    f
}

// ---------- analysis ----------

fn refusal_hits(text: &str) -> Vec<String> {
    driver::refusal_hits(text, REFUSALS)
}

fn commands_of(chat: &[Value], ws: &Path) -> Vec<Cmd> {
    let wsp = ws.to_string_lossy().to_string();
    let mut out = Vec::new();
    for i in chat.iter().filter(|i| i["kind"] == "tool") {
        let Some(cmd) = i["command"].as_str().filter(|c| !c.is_empty()) else { continue };
        let cwd = i["cwd"].as_str().unwrap_or("").to_string();
        let probe = cmd.replace(&wsp, "/workspace");
        let mut outside = Vec::new();
        if !cwd.is_empty() && !cwd.starts_with("/workspace") && !cwd.starts_with(&wsp) {
            outside.push(format!("cwd {cwd}"));
        }
        for (pat, label) in [("~/", "~/"), ("$HOME", "$HOME"), ("/Users/", "/Users/"), ("/root", "/root"), ("/home/", "/home/"), ("/mnt/", "/mnt/ (folder grant)"), ("/etc/", "/etc/"), ("/var/", "/var/"), ("/usr/local", "/usr/local"), ("sudo ", "sudo"), ("apt-get", "apt-get"), ("apt install", "apt")] {
            if probe.contains(pat) {
                outside.push(label.to_string());
            }
        }
        out.push(Cmd { cmd: short(cmd, 160), cwd, exit: i["exitCode"].as_i64(), outside, out_tail: i["output"].as_str().unwrap_or("").chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect() });
    }
    out
}

/// Kernel gaps (describe.kernel_gaps) the agent named: >= 2 keywords of the gap in one sentence that sounds like a limit.
fn gaps_named(desc: &Value, agent_text: &str) -> Vec<String> {
    let gaps: Vec<String> = desc["kernel_gaps"].as_array().map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect()).unwrap_or_default();
    let limit = ["gap", "not available", "no primitive", "missing", "not supported", "can't", "cannot", "don't have", "unable", "isn't possible", "no way"];
    let sentences: Vec<&str> = agent_text.split(['.', '\n', '!', '?']).collect();
    let mut out = Vec::new();
    for g in gaps {
        let head = g.split(['(', ':']).next().unwrap_or(&g).to_lowercase();
        let kws: Vec<String> = head.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 5).map(String::from).collect();
        if kws.is_empty() {
            continue;
        }
        let hit = sentences.iter().any(|s| kws.iter().filter(|w| s.contains(w.as_str())).count() >= kws.len().min(2) && limit.iter().any(|l| s.contains(l)));
        if hit {
            out.push(g);
        }
    }
    if agent_text.contains("kernel gap") {
        out.push("(agent said \"kernel gap\" — see summary)".into());
    }
    out
}

struct Eval {
    status: &'static str,
    reason: String,
    passed: usize,
    total: usize,
}

fn evaluate(checks: &[Check], f: &Facts, refusals: &[String], modules_created: usize) -> Eval {
    let req: Vec<&Check> = checks.iter().filter(|c| c.kind == Kind::Req).collect();
    let passed = req.iter().filter(|c| c.ok).count();
    let total = req.len();
    let loads = f.modules_loaded().iter().any(|(_, s)| s != "error");
    let services = f.service_names().len();
    if passed == total {
        return Eval { status: "pass", reason: "all required checks pass".into(), passed, total };
    }
    if modules_created == 0 && services == 0 {
        let why = if refusals.is_empty() { "nothing was built" } else { "refused / nothing built" };
        return Eval { status: "fail", reason: why.into(), passed, total };
    }
    if !loads && f.service_names().is_empty() {
        return Eval { status: "fail", reason: "modules exist but none loads".into(), passed, total };
    }
    if passed == 0 || (!refusals.is_empty() && passed * 2 < total) {
        return Eval { status: "fail", reason: format!("only {passed}/{total} required checks{}", if refusals.is_empty() { "" } else { " and the agent refused" }), passed, total };
    }
    let missing: Vec<&str> = req.iter().filter(|c| !c.ok).map(|c| c.name.as_str()).collect();
    Eval { status: "partial", reason: format!("missing: {}", short(&missing.join("; "), 260)), passed, total }
}

fn asked_in_text(text: &str) -> bool {
    let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    let t = tail.to_lowercase();
    tail.trim_end().ends_with('?') || ["let me know", "would you like", "do you want", "which would you", "please provide", "please confirm", "should i ", "once you "].iter().any(|p| t.contains(p))
}

// ---------- fixtures ----------

fn wav_bytes(freq: f32, secs: f32) -> Vec<u8> {
    let rate = 8000u32;
    let n = (rate as f32 * secs) as usize;
    let mut data = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = ((i as f32 / rate as f32 * freq * std::f32::consts::TAU).sin() * 8000.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

fn make_fixture(dir: &Path, name: &str) {
    if dir.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    match name {
        "Music" => {
            for (i, f) in [262.0, 330.0, 392.0].iter().enumerate() {
                let _ = std::fs::write(dir.join(format!("track-{}.wav", i + 1)), wav_bytes(*f, 1.0));
            }
        }
        _ => {
            let _ = std::fs::write(dir.join("README.md"), "# Readme\n\nA fixture document for the probe runner.\n");
            let _ = std::fs::write(dir.join("notes.md"), "# Notes\n\n- one\n- two\n");
            let _ = std::fs::write(dir.join("todo.txt"), "buy milk\n");
        }
    }
}

// ---------- one probe ----------

/// The scripted user of PROBES.md for one probe.
fn script(p: &'static Probe) -> Script {
    Script { answers: p.answers, typed: None, egress: Some(p.egress), consent: Some(p.consent) }
}

async fn run_probe(o: &Opts, p: &'static Probe) -> Res {
    let h = o.harness.as_str();
    let ws = o.ws_root.join(format!("{h}-{}", p.id));
    let _ = std::fs::remove_dir_all(&ws);
    let mut res = Res {
        id: p.id,
        status: "fail",
        reason: String::new(),
        ms: 0,
        turns: 0,
        modules: vec![],
        services: vec![],
        stats: Stats::default(),
        checks: vec![],
        commands: vec![],
        refusals: vec![],
        gaps_named: vec![],
        summary: String::new(),
        tools: 0,
        mcp_tools: 0,
        errors: vec![],
        module_status: vec![],
        suggestions: vec![],
        skipped: None,
    };
    if let Err(e) = driver::fresh_workspace(&ws) {
        res.skipped = Some(format!("workspace: {e}"));
        return res;
    }
    let sink = Arc::new(Collect::default());
    let k = match Kernel::new_opts(ws.clone(), None, Some(h), sink.clone()) {
        Ok(k) => k,
        Err(e) => {
            res.skipped = Some(format!("kernel: {e}"));
            return res;
        }
    };
    // folder grants: the panel cannot be clicked here, so the runner applies the grant before the VM starts
    for f in p.folders {
        let dir = folder_path(o, f);
        if !o.real_folders {
            make_fixture(&dir, f.home_dir);
        }
        match openapp_lib::primitives::files::grant_path(&k, &dir, Some(f.name), f.purpose, None, "user").await {
            Ok(_) => res.stats.needs_human.push(format!("folder panel for {} replaced by a programmatic grant of {} (-> /mnt/{})", f.home_dir, dir.display(), f.name)),
            Err(e) => res.stats.needs_human.push(format!("needs human: folder panel for {} (programmatic grant failed: {e})", f.home_dir)),
        }
    }
    if let Err(why) = driver::start_signed_in(&k, h).await {
        res.skipped = Some(why);
        return res;
    }
    let user = script(p);
    let before_mods: BTreeSet<String> = openapp_lib::workspace::module_dirs(&k.workspace).into_iter().collect();
    let sid = k.session_new();
    let t0 = Instant::now();
    let deadline = t0 + o.timeout;
    let mut handled: HashSet<String> = HashSet::new();
    let mut prompt = format!("/build {}", p.prompt);
    let mut followups = 0usize;
    loop {
        println!("  [{}] {} -> {}", p.id, if followups == 0 { "prompt" } else { "follow-up" }, short(&prompt, 90));
        if !user.turn(&k, &sid, &prompt, deadline, &mut res.stats, &mut handled).await {
            break;
        }
        // let cards that follow the turn (module loads, services) settle
        user.settle(&k, 3.0, &mut res.stats, &mut handled).await;
        let f = collect(&k);
        let mods_created = openapp_lib::workspace::module_dirs(&k.workspace).into_iter().filter(|m| !before_mods.contains(m)).count();
        let refusals = refusal_hits(&f.agent_text);
        let checks = (p.checks)(&f);
        let ev = evaluate(&checks, &f, &refusals, mods_created);
        let last_text = driver::agent_texts(&f.chat).last().cloned().unwrap_or_default();
        let asked = asked_in_text(&last_text);
        let hard_refusal = ev.status == "fail" && mods_created == 0 && !refusals.is_empty();
        let more = ev.status != "pass" && followups < o.followups && Instant::now() + Duration::from_secs(60) < deadline && !hard_refusal;
        println!("  [{}] turn ended: {} ({}/{} required checks){}", p.id, ev.status, ev.passed, ev.total, if more { " -> follow-up" } else { "" });
        if !more {
            break;
        }
        res.stats.followups.push(if asked { "agent asked in plain text".into() } else { "agent stopped before the end state".into() });
        followups += 1;
        prompt = FOLLOWUP.to_string();
    }
    // final settle: services reach running / waiting; cards from them are still answered
    for _ in 0..100 {
        user.handle_cards(&k, &mut res.stats, &mut handled);
        let starting = k.describe().pointer("/services/running").and_then(Value::as_array).is_some_and(|a| a.iter().any(|s| s["state"] == "starting"));
        if !starting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    user.handle_cards(&k, &mut res.stats, &mut handled);
    res.ms = t0.elapsed().as_millis();

    // final facts and evaluation
    let f = collect(&k);
    let created: Vec<String> = openapp_lib::workspace::module_dirs(&k.workspace).into_iter().filter(|m| !before_mods.contains(m)).collect();
    let agent_texts = driver::agent_texts(&f.chat);
    res.refusals = refusal_hits(&f.agent_text);
    res.checks = (p.checks)(&f);
    let ev = evaluate(&res.checks, &f, &res.refusals, created.len());
    res.status = if res.stats.timed_out && ev.status == "pass" { "pass" } else { ev.status };
    res.reason = if res.stats.timed_out { format!("{} (timed out after {} min)", ev.reason, o.timeout.as_secs() / 60) } else { ev.reason };
    res.turns = driver::turn_ends(&k.state_dir, &sid);
    res.modules = created;
    res.services = f.service_names();
    res.module_status = f.modules_loaded();
    res.summary = agent_texts.last().cloned().unwrap_or_default();
    res.commands = commands_of(&f.chat, &k.workspace);
    (res.tools, res.mcp_tools) = driver::tool_counts(&f.chat);
    res.errors = driver::errors_of(&f.chat);
    res.gaps_named = gaps_named(&f.desc, &f.agent_text);
    res.suggestions = suggest(p, &res, &f);
    // cards by kind from the chat for kinds the loop did not see (already counted); notifications for E9
    let notes = sink.notes.lock().unwrap_or_else(|e| e.into_inner()).clone();

    // raw transcript
    let raw = json!({
        "probe": p.id, "harness": h, "prompt": p.prompt, "status": res.status, "reason": res.reason, "ms": res.ms,
        "checks": res.checks.iter().map(|c| json!({ "name": c.name, "ok": c.ok, "kind": format!("{:?}", c.kind) })).collect::<Vec<_>>(),
        "stats": res.stats.to_json(),
        "commands": res.commands.iter().map(|c| json!({ "cmd": c.cmd, "cwd": c.cwd, "exit": c.exit, "outside": c.outside, "out_tail": c.out_tail })).collect::<Vec<_>>(),
        "refusals": res.refusals, "gaps_named": res.gaps_named, "suggestions": res.suggestions,
        "notifications": notes,
        "describe": f.desc, "snapshot_modules": f.snap["modules"], "placements": f.snap["layout"]["placements"], "presented": f.snap["presented"], "grants": f.snap["grants"],
        "manifests": f.manifests.iter().map(|(id, m)| json!({ "id": id, "manifest": m })).collect::<Vec<_>>(),
        "module_files": f.files.iter().filter(|(_, t)| t.len() <= 64 * 1024).take(60).map(|(p, t)| json!({ "path": p, "text": t })).collect::<Vec<_>>(),
        "research": f.research,
        "chat": f.chat,
    });
    RAW.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap_or_else(|e| e.into_inner()).push((p.id.to_string(), raw));

    driver::stop(&k).await;
    if !o.keep_ws {
        let _ = std::fs::remove_dir_all(&ws);
    }
    let _ = std::fs::remove_dir_all(openapp_lib::state::state_dir(&ws));
    res
}

static RAW: std::sync::OnceLock<Mutex<Vec<(String, Value)>>> = std::sync::OnceLock::new();

fn take_raw(id: &str) -> Option<Value> {
    let m = RAW.get()?;
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let i = g.iter().position(|(x, _)| x == id)?;
    Some(g.remove(i).1)
}

// ---------- next-primitive heuristics ----------

fn suggest(p: &Probe, r: &Res, f: &Facts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let outputs: String = f.chat.iter().filter(|i| i["kind"] == "tool").filter_map(|i| i["output"].as_str()).collect::<Vec<_>>().join("\n").to_lowercase();
    let corpus = format!("{}\n{}\n{}", f.agent_text, outputs, r.errors.join("\n").to_lowercase());
    let failed_cmds: Vec<&Cmd> = r.commands.iter().filter(|c| c.exit.is_some_and(|e| e != 0)).collect();
    if ["glibc_", "glibc", "ld-linux", "libc.so", "exec format error", "lib64/ld"].iter().any(|w| outputs.contains(w)) {
        out.push("G3: a binary failed to load (glibc / loader / architecture) -> glibc guest or amd64 compat".into());
    }
    for g in &r.gaps_named {
        out.push(format!("kernel gap named by the agent: {g}"));
    }
    if !r.stats.egress_denied.is_empty() {
        out.push(format!("egress: hosts denied by the probe policy: {} -> add to the default allowlist or to this probe's approvals", r.stats.egress_denied.iter().take(8).cloned().collect::<Vec<_>>().join(", ")));
    }
    if corpus.contains("grant_folder") && (corpus.contains("no app") || corpus.contains("panel") || corpus.contains("cancel")) && p.folders.is_empty() {
        out.push("G5: the agent tried files.grant_folder; a panel cannot be clicked by the runner -> programmatic grant or a folder card".into());
    }
    if !r.stats.consent_denied.is_empty() {
        out.push(format!("consent cards denied (not in the probe list): {}", r.stats.consent_denied.iter().take(4).map(|s| short(s, 80)).collect::<Vec<_>>().join("; ")));
    }
    // failed required checks -> primitive
    let map: &[(&str, &str)] = &[
        ("host module", "G8: long-running host module (service: true) or the agent did not use a host module"),
        ("avaudioengine", "G8: long-running host module with audio on the Mac"),
        ("tray", "G4: tray.set"),
        ("shortcut", "G4: shortcut.register"),
        ("job", "G6: jobs registry (cron / watch)"),
        ("agent module", "G10: agent modules standard"),
        ("secret release", "G7: secret.release"),
        ("display", "G3: virtual display"),
        ("pty", "G2: PTY"),
        ("xterm", "G2: PTY + xterm view"),
        ("service", "G2: services"),
        ("channel", "G1: live channels"),
        ("files.save", "G5: files.save"),
        ("folder", "G5: folder grants"),
        ("reveal", "G5: files.reveal"),
        ("notification", "G4: notify with actions"),
        ("now playing", "G8: host module for Now Playing / media keys"),
        ("oasvc", "G2: exposed ports (oasvc://)"),
    ];
    for c in r.checks.iter().filter(|c| c.kind == Kind::Req && !c.ok) {
        let n = c.name.to_lowercase();
        if let Some((_, s)) = map.iter().find(|(k, _)| n.contains(k)) {
            let line = format!("{s} (check failed: {})", short(&c.name, 70));
            if !out.contains(&line) {
                out.push(line);
            }
        }
    }
    if failed_cmds.len() >= 3 {
        out.push(format!("tooling: {} shell commands failed; first: `{}` (exit {:?})", failed_cmds.len(), failed_cmds[0].cmd, failed_cmds[0].exit));
    }
    if r.stats.timed_out {
        out.push("budget: the probe hit its timeout -> split the request or raise --timeout-min".into());
    }
    if r.status != "pass" && asked_in_text(&r.summary) && !r.stats.followups.is_empty() {
        out.push("agent still waits on the user after the follow-ups -> builder skill should say what to do without answers".into());
    }
    if r.module_status.iter().any(|(_, s)| s == "error") {
        out.push("a module ends in status error -> check the diagnostics the agent saw (module_load errors)".into());
    }
    if r.modules.is_empty() && r.services.is_empty() && r.status == "fail" {
        out.push("nothing built: the agent had no path -> name the missing primitive from its summary".into());
    }
    out
}

// ---------- report ----------

fn write_report(o: &Opts, n: u32, started: &str, results: &[Res]) -> PathBuf {
    let mut md = format!(
        "# Probe loop {n} — {started}\n\nRunner: `src-tauri/examples/eval.rs probes` (harness `{}`, build mode, fresh workspace + kernel + session per probe). VM state: `{}`. Per-probe timeout {} min, up to {} follow-ups (\"{FOLLOWUP}\"). Questions answered from the probe's list (fallback: first option), egress allowed only for the probe's hosts, listed consent cards allowed, secret cards cancelled. Folder grants are applied programmatically before the VM starts ({}).\n\n",
        o.harness,
        vm_home_of(o).display(),
        o.timeout.as_secs() / 60,
        o.followups,
        if o.real_folders { "real folders" } else { "fixture copies, not the real folders" }
    );
    md.push_str("## Summary\n\n| Probe | Status | Time (s) | Turns | Questions | Cards | Modules | Services | Shell cmds (outside) | Refusal phrases | Checks |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in results {
        if let Some(s) = &r.skipped {
            md.push_str(&format!("| {} | skipped | — | — | — | — | — | — | — | — | {} |\n", r.id, short(s, 80)));
            continue;
        }
        let req: Vec<&Check> = r.checks.iter().filter(|c| c.kind == Kind::Req).collect();
        let cards: u32 = r.stats.cards.values().sum();
        md.push_str(&format!(
            "| {} | **{}** | {} | {} | {} | {} | {} | {} | {} ({}) | {} | {}/{} |\n",
            r.id,
            r.status,
            r.ms / 1000,
            r.turns,
            r.stats.cards.get("question").copied().unwrap_or(0),
            cards,
            r.modules.len(),
            r.services.len(),
            r.commands.len(),
            r.commands.iter().filter(|c| !c.outside.is_empty()).count(),
            r.refusals.len(),
            req.iter().filter(|c| c.ok).count(),
            req.len()
        ));
    }
    md.push_str("\n## Details\n");
    for r in results {
        let p = PROBES.iter().find(|p| p.id == r.id).unwrap();
        md.push_str(&format!("\n### {} · {} — {}\n\n> {}\n\n", r.id, p.title, r.status, p.prompt));
        if let Some(s) = &r.skipped {
            md.push_str(&format!("Skipped: {s}\n"));
            continue;
        }
        md.push_str(&format!("- Why: {}\n- End state wanted: {}\n- Human-only steps allowed: {}\n", r.reason, p.end_state, p.human));
        md.push_str("- Checks:\n");
        for c in &r.checks {
            let mark = match c.kind {
                Kind::Manual => "~".to_string(),
                _ => if c.ok { "x".to_string() } else { " ".to_string() },
            };
            md.push_str(&format!("  - [{mark}] {}{}\n", c.name, match c.kind { Kind::Opt => " (optional)", Kind::Manual => " (needs a human, not scored)", Kind::Req => "" }));
        }
        let cards = r.stats.cards.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ");
        md.push_str(&format!("- Cards: {}\n", if cards.is_empty() { "—".into() } else { cards }));
        md.push_str(&format!("- Questions: {}\n", list(&r.stats.questions, 8)));
        md.push_str(&format!("- Question fallbacks (first option used): {}\n", list(&r.stats.fallbacks, 8)));
        md.push_str(&format!("- Secret cards cancelled: {}\n", list(&r.stats.secrets, 8)));
        md.push_str(&format!("- Egress allowed: {} · denied: {}\n", list(&r.stats.egress_allowed, 8), list(&r.stats.egress_denied, 8)));
        md.push_str(&format!("- Consent decisions: {}\n", list(&r.stats.decisions.iter().filter(|d| !d.starts_with("egress")).cloned().collect::<Vec<_>>(), 8)));
        md.push_str(&format!("- Consent denied: {}\n", list(&r.stats.consent_denied, 6)));
        md.push_str(&format!("- Harness approval cards (allowed once): {}\n", list(&r.stats.harness_cards, 6)));
        md.push_str(&format!("- Follow-ups sent: {} {}\n", r.stats.followups.len(), if r.stats.followups.is_empty() { String::new() } else { format!("({})", r.stats.followups.join("; ")) }));
        md.push_str(&format!("- Needs human / runner substitutions: {}\n", list(&r.stats.needs_human, 4)));
        md.push_str(&format!("- Modules created: {} (status: {})\n", list(&r.modules, 12), r.module_status.iter().map(|(i, s)| format!("{i}={s}")).collect::<Vec<_>>().join(", ")));
        md.push_str(&format!("- Services: {}\n", list(&r.services, 12)));
        md.push_str(&format!("- Tool calls: {} (openapp MCP: {}) · shell commands: {} · outside /workspace: {}\n", r.tools, r.mcp_tools, r.commands.len(), r.commands.iter().filter(|c| !c.outside.is_empty()).count()));
        let outside: Vec<String> = r.commands.iter().filter(|c| !c.outside.is_empty()).map(|c| format!("{} [{}]", c.cmd, c.outside.join(", "))).collect();
        md.push_str(&format!("- Commands touching outside /workspace: {}\n", list(&outside, 8)));
        let failed: Vec<String> = r.commands.iter().filter(|c| c.exit.is_some_and(|e| e != 0)).map(|c| format!("{} (exit {:?})", c.cmd, c.exit)).collect();
        md.push_str(&format!("- Failed commands: {}\n", list(&failed, 6)));
        md.push_str(&format!("- Refusal phrases: {}\n", list(&r.refusals, 5)));
        md.push_str(&format!("- Kernel gaps named by the agent: {}\n", list(&r.gaps_named, 5)));
        md.push_str(&format!("- Errors: {}\n", list(&r.errors, 4)));
        md.push_str(&format!("- Next primitive hints: {}\n", if r.suggestions.is_empty() { "—".into() } else { format!("\n{}", r.suggestions.iter().map(|s| format!("  - {s}")).collect::<Vec<_>>().join("\n")) }));
        md.push_str(&format!("- Final summary:\n\n  > {}\n", short(&r.summary, 1500)));
    }
    // aggregate suggestions
    let mut by: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for r in results {
        for s in &r.suggestions {
            let key = s.split(" (check failed").next().unwrap_or(s).to_string();
            by.entry(key).or_default().push(r.id);
        }
    }
    md.push_str("\n## Suggested next primitive (all probes)\n\n");
    if by.is_empty() {
        md.push_str("No hints. Every probe passed or was skipped.\n");
    } else {
        let mut rows: Vec<(String, Vec<&str>)> = by.into_iter().collect();
        rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (s, ids) in rows {
            md.push_str(&format!("- {s} — probes {}\n", ids.join(", ")));
        }
    }
    md.push_str(&format!("\nRaw transcripts: `eval/probes/raw/{n}/<id>.json` (chat, research log, describe, manifests, module sources).\n"));
    let path = out_dir().join(format!("LOOP-{n}.md"));
    let _ = std::fs::write(&path, md);
    path
}

async fn run(o: Opts, n: u32) -> anyhow::Result<()> {
    let ps = selected(&o);
    let started = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    println!("vm home: {}  workspaces: {}  harness: {}  probes: {:?}  loop {n}", openapp_lib::vm::vm_home().display(), o.ws_root.display(), o.harness, ps.iter().map(|p| p.id).collect::<Vec<_>>());
    let raw_dir = out_dir().join("raw").join(n.to_string());
    let _ = std::fs::create_dir_all(&raw_dir);
    let _ = std::fs::write(out_dir().join(".gitignore"), "raw/\n");
    let mut results: Vec<Res> = Vec::new();
    for p in ps {
        println!("== {} {}", p.id, p.title);
        let r = run_probe(&o, p).await;
        match &r.skipped {
            Some(s) => println!("== {} skipped: {s}", p.id),
            None => println!("== {} {} in {} s ({})", p.id, r.status, r.ms / 1000, r.reason),
        }
        if let Some(raw) = take_raw(p.id) {
            let _ = std::fs::write(raw_dir.join(format!("{}.json", p.id)), serde_json::to_string_pretty(&raw).unwrap_or_default());
        }
        let skipped = r.skipped.is_some();
        results.push(r);
        let path = write_report(&o, n, &started, &results);
        println!("   report: {}", path.display());
        if skipped && results.len() == 1 {
            // the harness cannot run at all in this VM state: do not repeat the boot for every probe
            println!("first probe could not start; stopping");
            break;
        }
    }
    Ok(())
}
