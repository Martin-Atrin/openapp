//! G6 jobs registry (contract §21).
//!
//! - Manifest `"jobs": [{ name, schedule, action? }]` or runtime `job.create / job.update / job.delete / job.run_now`.
//!   `schedule`: a cron expression (5 fields = minute-precision, local time; 6–7 fields with seconds; `@daily` …),
//!   `{ "every": "15m" }` (s/m/h/d, combinable: "1h30m"), `{ "watch": "notes/**/*.md" }` (workspace path or glob,
//!   polled every tick; `/workspace/…` accepted) or `{ "webhook": true }` (local URL with a token in the path:
//!   `http://127.0.0.1:<port>/hook/<token>` on the Mac, `http://127.0.0.1:7801/hook/<token>` in the VM).
//! - `action`: `{ tool, args? }` (module tool), `{ host, … }` (host action as the module), `{ agent, input? }`
//!   (G10 agent of the module). Agents whose `triggers` list the job name also run. Watch/webhook runs pass `input`
//!   (`{changed: [...]}` / `{method, query, body}`) as `args.input` / the agent input.
//! - A run activates the module (G9 `onJob`), is logged (start, end, status, ms, output tail) in
//!   `<state>/jobs/<module>.<name>.jsonl` and the research log (no output), and published on `kernel/jobs`.
//!   Host data `jobs`; describe `jobs`. One run per job at a time.
//! - Missed runs: the next due time is computed from the last run (persisted in `<state>/jobs.json`); a job that is
//!   overdue after sleep or while OpenApp was closed runs once (trigger `missed`), not once per missed slot. Sleep is
//!   detected from the wall clock moving faster than the monotonic clock between ticks (research `jobs_wake`).
//!   `OPENAPP_JOBS_TICK_MS` (default 1000) sets the tick.

use crate::research::Kind;
use crate::util::MutexExt;
use super::channels::{self, ChannelData};
use super::*;
use crate::modules::{safe_rel, valid_module_id};
use chrono::{DateTime, Local};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

const OUTPUT_TAIL: usize = 2000;
const MISSED_GRACE_S: i64 = 60;
const WATCH_MAX_FILES: usize = 5000;
const LOG_MAX: u64 = 1024 * 1024;

pub fn tick() -> Duration {
    std::env::var("OPENAPP_JOBS_TICK_MS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v >= 50).map(Duration::from_millis).unwrap_or(Duration::from_secs(1))
}

fn job_timeout() -> Duration {
    std::env::var("OPENAPP_JOB_TIMEOUT_S").ok().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0).map(Duration::from_secs).unwrap_or(Duration::from_secs(600))
}

// ---------- schedules ----------

#[derive(Clone, Debug)]
pub enum Sched {
    Cron(Box<cron::Schedule>, String),
    Every(Duration, String),
    Watch(String),
    Webhook,
}

impl Sched {
    pub fn label(&self) -> String {
        match self {
            Sched::Cron(_, e) => format!("cron {e}"),
            Sched::Every(_, s) => format!("every {s}"),
            Sched::Watch(p) => format!("watch {p}"),
            Sched::Webhook => "webhook".into(),
        }
    }
    fn icon(&self) -> &'static str {
        match self {
            Sched::Cron(..) | Sched::Every(..) => "clock",
            Sched::Watch(_) => "eye",
            Sched::Webhook => "webhook",
        }
    }
}

/// "15m", "30s", "1h30m", "1d" → Duration (≥ 1 s).
pub fn parse_every(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let mut total = 0u64;
    let mut num = String::new();
    let mut any = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().map_err(|_| format!("bad interval \"{s}\" (e.g. \"15m\", \"30s\", \"1h30m\")"))?;
        num.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return Err(format!("bad interval unit '{c}' in \"{s}\" (s, m, h, d)")),
        };
        any = true;
    }
    if !num.is_empty() || !any || total == 0 {
        return Err(format!("bad interval \"{s}\" (e.g. \"15m\", \"30s\", \"1h30m\")"));
    }
    Ok(Duration::from_secs(total))
}

pub fn parse_sched(v: &Value) -> Result<Sched, String> {
    match v {
        Value::String(e) => {
            let e = e.trim();
            let fields = e.split_whitespace().count();
            let full = if e.starts_with('@') || fields >= 6 { e.to_string() } else if fields == 5 { format!("0 {e}") } else {
                return Err(format!("cron \"{e}\" needs 5 fields (minute hour day month weekday), e.g. \"0 8 * * *\""));
            };
            let s = cron::Schedule::from_str(&full).map_err(|x| format!("bad cron \"{e}\": {x}"))?;
            Ok(Sched::Cron(Box::new(s), e.to_string()))
        }
        Value::Object(o) => {
            if let Some(ev) = o.get("every") {
                let s = ev.as_str().ok_or("\"every\" must be a string like \"15m\"")?;
                return Ok(Sched::Every(parse_every(s)?, s.to_string()));
            }
            if let Some(w) = o.get("watch") {
                let p = w.as_str().ok_or("\"watch\" must be a workspace path or glob")?;
                let rel = p.strip_prefix("/workspace/").unwrap_or(p).trim_start_matches("./");
                if !safe_rel(rel) || rel.starts_with(".openapp") {
                    return Err(format!("\"watch\" path \"{p}\" must be inside the workspace (relative, no \"..\")"));
                }
                return Ok(Sched::Watch(rel.to_string()));
            }
            if o.get("webhook").and_then(Value::as_bool) == Some(true) {
                return Ok(Sched::Webhook);
            }
            Err("schedule object needs \"every\", \"watch\" or \"webhook\": true".into())
        }
        _ => Err("\"schedule\" must be a cron string or { every | watch | webhook }".into()),
    }
}

/// Next run after `anchor` (cron, every); None for watch/webhook.
pub fn next_after(s: &Sched, anchor: DateTime<Local>) -> Option<DateTime<Local>> {
    match s {
        Sched::Cron(c, _) => c.after(&anchor).next(),
        Sched::Every(d, _) => Some(anchor + chrono::Duration::from_std(*d).ok()?),
        _ => None,
    }
}

/// Due check: None (not yet), Some(false) on time, Some(true) missed (overdue more than the grace, e.g. after
/// sleep). One run either way: the anchor moves to the run time, so missed slots are never replayed one by one.
pub fn due(s: &Sched, anchor: DateTime<Local>, now: DateTime<Local>) -> Option<bool> {
    let next = next_after(s, anchor)?;
    if now < next {
        return None;
    }
    Some((now - next).num_seconds() > MISSED_GRACE_S)
}

/// Seconds the machine slept between two ticks: wall-clock delta minus monotonic delta (Instant stops in sleep).
pub fn slept(wall_prev: SystemTime, wall_now: SystemTime, mono: Duration) -> Duration {
    wall_now.duration_since(wall_prev).unwrap_or_default().saturating_sub(mono)
}

// ---------- glob ----------

/// Glob over '/'-separated paths: `**` (any depth), `*`, `?`.
pub fn glob_match(pat: &str, path: &str) -> bool {
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => seg(&p[1..], s) || (!s.is_empty() && seg(p, &s[1..])),
            (Some(b'?'), Some(_)) => seg(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => seg(&p[1..], &s[1..]),
            _ => false,
        }
    }
    fn parts(p: &[&str], s: &[&str]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(&"**") => parts(&p[1..], s) || (!s.is_empty() && parts(p, &s[1..])),
            Some(x) => !s.is_empty() && seg(x.as_bytes(), s[0].as_bytes()) && parts(&p[1..], &s[1..]),
        }
    }
    let p: Vec<&str> = pat.split('/').filter(|x| !x.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    parts(&p, &s)
}

fn has_wild(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// Files matched by a watch spec: path → (mtime, len).
fn watch_scan(ws: &Path, spec: &str) -> BTreeMap<String, (SystemTime, u64)> {
    let mut out = BTreeMap::new();
    let segs: Vec<&str> = spec.split('/').collect();
    let base_n = segs.iter().take_while(|s| !has_wild(s)).count();
    let base_rel = segs[..base_n].join("/");
    let base = ws.join(&base_rel);
    let pattern = if base_n == segs.len() {
        if base.is_dir() { format!("{base_rel}/**") } else { base_rel.clone() }
    } else {
        spec.to_string()
    };
    if base.is_file() {
        if let Ok(m) = std::fs::metadata(&base) {
            out.insert(base_rel, (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        }
        return out;
    }
    let mut stack = vec![(base, 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == ".openapp" || name == "node_modules" || name == ".git" || name == ".DS_Store" {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                if depth < 10 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let rel = p.strip_prefix(ws).unwrap_or(&p).to_string_lossy().to_string();
            if glob_match(&pattern, &rel)
                && let Ok(m) = e.metadata()
            {
                out.insert(rel, (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
                if out.len() >= WATCH_MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

fn changes(old: &BTreeMap<String, (SystemTime, u64)>, new: &BTreeMap<String, (SystemTime, u64)>) -> Vec<Value> {
    let mut out = Vec::new();
    for (p, v) in new {
        match old.get(p) {
            None => out.push(json!({ "path": p, "change": "added" })),
            Some(o) if o != v => out.push(json!({ "path": p, "change": "modified" })),
            _ => {}
        }
    }
    for p in old.keys().filter(|p| !new.contains_key(*p)) {
        out.push(json!({ "path": p, "change": "removed" }));
    }
    out
}

// ---------- registry ----------

#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub module: String,
    pub name: String,
    pub sched: Sched,
    pub schedule: Value,
    pub action: Option<Value>,
    pub source: &'static str,
    pub enabled: bool,
}

/// A watch job's pattern and the files it saw (mtime, size).
type WatchState = (String, BTreeMap<String, (SystemTime, u64)>);

#[derive(Default)]
pub struct Jobs {
    /// runtime jobs: id → { module, name, schedule, action, enabled }
    runtime: Mutex<BTreeMap<String, Value>>,
    /// persisted per-job state: id → { created, last_run, last_status, last_ms, last_error, runs, webhook }
    st: Mutex<BTreeMap<String, Value>>,
    running: Mutex<HashSet<String>>,
    watch: Mutex<HashMap<String, WatchState>>,
    recent: Mutex<std::collections::VecDeque<Value>>,
    problems: Mutex<BTreeMap<String, Vec<String>>>,
    loaded: AtomicBool,
    started: AtomicBool,
    io: Mutex<()>,
}

pub fn jobs(k: &Kernel) -> Arc<Jobs> {
    let j = k.prims.get::<Jobs>();
    if !j.loaded.swap(true, Ordering::SeqCst) {
        let v: Value = std::fs::read(k.state_dir.join("jobs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Some(r) = v["runtime"].as_object() {
            *j.runtime.locked() = r.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        }
        if let Some(s) = v["state"].as_object() {
            *j.st.locked() = s.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
        }
    }
    j
}

fn save(k: &Kernel) {
    let j = jobs(k);
    let _g = j.io.locked();
    let v = json!({
        "runtime": *j.runtime.locked(),
        "state": *j.st.locked(),
    });
    let p = k.state_dir.join("jobs.json");
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, p);
    }
}

fn valid_job_name(n: &str) -> bool {
    crate::util::valid_ident(n, 40)
}

fn validate_action(a: &Value) -> Result<(), String> {
    let o = a.as_object().ok_or("\"action\" must be an object: { tool, args? } | { host, … } | { agent, input? }")?;
    if let Some(t) = o.get("tool") {
        if t.as_str().is_none_or(|s| s.is_empty()) {
            return Err("action.tool must be a tool name of the module".into());
        }
        if o.get("args").is_some_and(|x| !x.is_object()) {
            return Err("action.args must be an object".into());
        }
        return Ok(());
    }
    if o.get("host").and_then(Value::as_str).is_some() || o.get("agent").and_then(Value::as_str).is_some() {
        return Ok(());
    }
    Err("\"action\" needs \"tool\", \"host\" or \"agent\"".into())
}

fn parse_job(module: &str, v: &Value, source: &'static str) -> Result<Job, String> {
    let name = v["name"].as_str().ok_or("job needs \"name\"")?;
    if !valid_job_name(name) {
        return Err(format!("job name \"{name}\" must match [A-Za-z0-9_-]{{1,40}}"));
    }
    let schedule = v.get("schedule").cloned().ok_or_else(|| format!("job \"{name}\" needs \"schedule\""))?;
    let sched = parse_sched(&schedule).map_err(|e| format!("job \"{name}\": {e}"))?;
    let action = v.get("action").filter(|a| !a.is_null()).cloned();
    if let Some(a) = &action {
        validate_action(a).map_err(|e| format!("job \"{name}\": {e}"))?;
    }
    Ok(Job { id: format!("{module}/{name}"), module: module.into(), name: name.into(), sched, schedule, action, source, enabled: v["enabled"].as_bool().unwrap_or(true) })
}

/// All jobs of loaded (running or idle) modules: manifest + runtime. Records manifest problems.
pub fn all_jobs(k: &Kernel) -> Vec<Job> {
    let mods: Vec<(String, Value)> = k
        .lock()
        .modules
        .values()
        .filter(|m| m.status == "running" || m.status == "idle")
        .filter_map(|m| m.manifest.as_ref().map(|mf| (m.id.clone(), mf.raw.clone())))
        .collect();
    let j = jobs(k);
    let mut out = Vec::new();
    let mut problems: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, raw) in &mods {
        match raw.get("jobs") {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => {
                for v in a {
                    match parse_job(id, v, "manifest") {
                        Ok(job) => out.push(job),
                        Err(e) => problems.entry(id.clone()).or_default().push(format!("modules/{id}/openapp.json jobs: {e}")),
                    }
                }
            }
            Some(_) => problems.entry(id.clone()).or_default().push(format!("modules/{id}/openapp.json: \"jobs\" must be an array")),
        }
    }
    let ids: HashSet<String> = mods.iter().map(|(i, _)| i.clone()).collect();
    for (_, v) in j.runtime.locked().iter() {
        let m = v["module"].as_str().unwrap_or("");
        if !ids.contains(m) {
            continue;
        }
        if let Ok(job) = parse_job(m, v, "runtime")
            && !out.iter().any(|x| x.id == job.id)
        {
            out.push(job);
        }
    }
    *j.problems.locked() = problems;
    out
}

pub fn count(k: &Kernel) -> usize {
    all_jobs(k).iter().filter(|j| j.enabled).count()
}

fn find(k: &Kernel, id: &str) -> Option<Job> {
    all_jobs(k).into_iter().find(|j| j.id == id)
}

fn state_of(k: &Kernel, id: &str) -> Value {
    jobs(k).st.locked().get(id).cloned().unwrap_or(Value::Null)
}

fn patch_state(k: &Kernel, id: &str, f: impl FnOnce(&mut Map<String, Value>)) {
    {
        let j = jobs(k);
        let mut st = j.st.locked();
        let e = st.entry(id.to_string()).or_insert_with(|| json!({}));
        if !e.is_object() {
            *e = json!({});
        }
        if let Some(o) = e.as_object_mut() {
            f(o);
        }
    }
    save(k);
}

fn parse_ts(v: &Value) -> Option<DateTime<Local>> {
    v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Local))
}

/// Ensure created/webhook state exists for every job; returns the webhook token map.
fn ensure_state(k: &Kernel, list: &[Job]) {
    let mut changed = false;
    {
        let j = jobs(k);
        let mut st = j.st.locked();
        for job in list {
            let e = st.entry(job.id.clone()).or_insert_with(|| {
                changed = true;
                json!({ "created": Local::now().to_rfc3339(), "runs": 0 })
            });
            if matches!(job.sched, Sched::Webhook) && e.get("webhook").and_then(Value::as_str).is_none() {
                e["webhook"] = json!(format!("wh_{}", uuid::Uuid::new_v4().simple()));
                changed = true;
            }
        }
    }
    if changed {
        save(k);
    }
}

pub fn webhook_url(k: &Kernel, token: &str, in_vm: bool) -> String {
    let port = if in_vm { crate::vm::MCP_PORT } else { k.mcp.get().map(|e| e.port).unwrap_or(0) };
    format!("http://127.0.0.1:{port}/hook/{token}")
}

fn job_json(k: &Kernel, job: &Job) -> Value {
    let st = state_of(k, &job.id);
    let running = jobs(k).running.locked().contains(&job.id);
    let anchor = parse_ts(&st["last_run"]).or_else(|| parse_ts(&st["created"])).unwrap_or_else(Local::now);
    let next = next_after(&job.sched, anchor).map(|d| d.to_rfc3339());
    let mut v = json!({
        "id": job.id, "module": job.module, "name": job.name, "schedule": job.schedule, "kind": job.sched.label(),
        "action": job.action, "source": job.source, "enabled": job.enabled, "running": running,
        "last_run": st["last_run"], "last_status": st["last_status"], "last_ms": st["last_ms"], "last_error": st["last_error"],
        "runs": st["runs"], "next": next,
        "log": format!("{}/jobs/{}.{}.jsonl", k.state_dir.display(), job.module, job.name),
    });
    if let Some(t) = st["webhook"].as_str() {
        v["url"] = json!(webhook_url(k, t, false));
        v["vm_url"] = json!(webhook_url(k, t, true));
    }
    v
}

pub fn list(k: &Kernel) -> Vec<Value> {
    let l = all_jobs(k);
    ensure_state(k, &l);
    l.iter().map(|j| job_json(k, j)).collect()
}

fn ago(ts: &Value) -> String {
    let Some(t) = parse_ts(ts) else { return "never".into() };
    let s = (Local::now() - t).num_seconds().max(0);
    if s < 60 { format!("{s}s ago") } else if s < 3600 { format!("{} min ago", s / 60) } else if s < 86400 { format!("{} h ago", s / 3600) } else { t.format("%Y-%m-%d %H:%M").to_string() }
}

fn host_items(k: &Kernel) -> Value {
    let l = all_jobs(k);
    Value::Array(
        l.iter()
            .map(|job| {
                let v = job_json(k, job);
                let last = if v["last_run"].is_null() { "never ran".to_string() } else { format!("last {} {}", v["last_status"].as_str().unwrap_or("?"), ago(&v["last_run"])) };
                let next = v["next"].as_str().and_then(|n| DateTime::parse_from_rfc3339(n).ok()).map(|n| format!(" · next {}", n.with_timezone(&Local).format("%H:%M:%S"))).unwrap_or_default();
                let status = if v["running"] == true { "running" } else if !job.enabled { "disabled" } else { "scheduled" };
                json!({ "id": job.id, "title": job.name, "subtitle": format!("{} · {} · {last}{next}", job.module, job.sched.label()), "icon": job.sched.icon(), "status": status,
                    "module": job.module, "name": job.name, "last_status": v["last_status"], "last_run": v["last_run"], "next": v["next"], "url": v["url"],
                    "actions": [{ "label": "Run now", "action": { "host": "job.run_now", "id": job.id } }] })
            })
            .collect(),
    )
}

// ---------- running ----------

fn append_log(k: &Kernel, job: &Job, v: &Value) {
    let dir = k.state_dir.join("jobs");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("{}.{}.jsonl", job.module, job.name));
    if std::fs::metadata(&p).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(&p, p.with_extension("jsonl.1"));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{v}");
    }
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    if n <= OUTPUT_TAIL { s.to_string() } else { s.chars().skip(n - OUTPUT_TAIL).collect() }
}

async fn execute(k: &Arc<Kernel>, job: &Job, trigger: &str, input: &Value) -> Result<String, String> {
    // G9: a job activates its module
    let idle = k.lock().modules.get(&job.module).is_some_and(|m| m.status == "idle");
    if idle {
        super::lifecycle::activate(k, &job.module, "onJob").await?;
    }
    super::lifecycle::touch(k, &job.module);
    let mut out = Vec::new();
    let ctx = json!({ "job": job.name, "trigger": trigger });
    if let Some(a) = &job.action {
        if let Some(tool) = a["tool"].as_str() {
            let mut args = a.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
            if !input.is_null() {
                args.insert("input".into(), input.clone());
            }
            let r = k.call_module_tool(&job.module, tool, args).await?;
            k.after_module_call(&job.module, &r, false);
            let text = crate::modules::result_text(&r);
            if r.is_error == Some(true) {
                return Err(format!("{tool}: {text}"));
            }
            out.push(text);
        } else if let Some(host) = a["host"].as_str() {
            let mut args = a.clone();
            if let Some(o) = args.as_object_mut() {
                o.remove("host");
                if !input.is_null() {
                    o.insert("input".into(), input.clone());
                }
            }
            let r = call_action(k, host, args, Caller::new(format!("module:{}", job.module))).await.ok_or_else(|| format!("unknown host action \"{host}\""))??;
            out.push(r.to_string());
        } else if let Some(agent) = a["agent"].as_str() {
            let inp = if input.is_null() { a.get("input").cloned().unwrap_or(Value::Null) } else { input.clone() };
            let r = super::agents::run_wait(k, &job.module, agent, inp, &format!("job:{}", job.name), ctx.clone()).await?;
            out.push(r["text"].as_str().map(String::from).unwrap_or_else(|| r.to_string()));
        }
    }
    // agents whose triggers name this job
    let triggered = super::agents::triggered_by(k, &job.module, &job.name);
    for agent in &triggered {
        if job.action.as_ref().and_then(|a| a["agent"].as_str()) == Some(agent.as_str()) {
            continue;
        }
        let r = super::agents::run_wait(k, &job.module, agent, input.clone(), &format!("job:{}", job.name), ctx.clone()).await?;
        out.push(format!("[{agent}] {}", r["text"].as_str().unwrap_or("")));
    }
    if job.action.is_none() && triggered.is_empty() {
        return Err(format!("job {} has no action and no agent lists it in \"triggers\"", job.id));
    }
    Ok(out.join("\n"))
}

/// Run a job now. Returns the run record (status ok | error).
pub async fn run_job(k: &Arc<Kernel>, id: &str, trigger: &str, input: Value) -> Result<Value, String> {
    let job = find(k, id).ok_or_else(|| format!("no job \"{id}\""))?;
    if !jobs(k).running.locked().insert(id.to_string()) {
        return Err(format!("job {id} is still running"));
    }
    let run = format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let started = Local::now();
    patch_state(k, id, |s| {
        s.insert("last_run".into(), json!(started.to_rfc3339()));
    });
    let start = json!({ "op": "start", "job": id, "run": run, "trigger": trigger, "ts": started.to_rfc3339() });
    append_log(k, &job, &start);
    channels::publish(k, "kernel/jobs", ChannelData::Json(start));
    k.research.log(&format!("module:{}", job.module), Kind::JobRun, json!({ "phase": "start", "job": id, "run": run, "trigger": trigger }));
    k.emit_snapshot();
    let t0 = Instant::now();
    let res = match tokio::time::timeout(job_timeout(), execute(k, &job, trigger, &input)).await {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {} s", job_timeout().as_secs())),
    };
    let ms = t0.elapsed().as_millis() as u64;
    let (status, output, error) = match &res {
        Ok(o) => ("ok", tail(o), None),
        Err(e) => ("error", String::new(), Some(tail(e))),
    };
    jobs(k).running.locked().remove(id);
    patch_state(k, id, |s| {
        s.insert("last_status".into(), json!(status));
        s.insert("last_ms".into(), json!(ms));
        s.insert("last_error".into(), json!(error));
        let n = s.get("runs").and_then(Value::as_u64).unwrap_or(0) + 1;
        s.insert("runs".into(), json!(n));
    });
    let end = json!({ "op": "end", "job": id, "run": run, "trigger": trigger, "status": status, "ms": ms, "output_tail": output, "error": error, "ts": Local::now().to_rfc3339() });
    append_log(k, &job, &end);
    channels::publish(k, "kernel/jobs", ChannelData::Json(end.clone()));
    k.research.log(&format!("module:{}", job.module), Kind::JobRun, json!({ "phase": "end", "job": id, "run": run, "trigger": trigger, "status": status, "ms": ms }));
    {
        let j = jobs(k);
        let mut r = j.recent.locked();
        r.push_back(end.clone());
        while r.len() > 50 {
            r.pop_front();
        }
    }
    k.emit_snapshot();
    Ok(end)
}

fn spawn_run(k: &Arc<Kernel>, id: String, trigger: &'static str, input: Value) {
    let k2 = k.clone();
    k.spawn(async move {
        if let Err(e) = run_job(&k2, &id, trigger, input).await {
            log::info!("job {id}: {e}");
        }
    });
}

async fn tick_once(k: &Arc<Kernel>) {
    if super::lifecycle::safe_mode(k) {
        return;
    }
    let list = all_jobs(k);
    ensure_state(k, &list);
    let now = Local::now();
    let running: HashSet<String> = jobs(k).running.locked().clone();
    for job in list.iter().filter(|j| j.enabled && !running.contains(&j.id)) {
        match &job.sched {
            Sched::Cron(..) | Sched::Every(..) => {
                let st = state_of(k, &job.id);
                let anchor = parse_ts(&st["last_run"]).or_else(|| parse_ts(&st["created"])).unwrap_or(now);
                if let Some(missed) = due(&job.sched, anchor, now) {
                    spawn_run(k, job.id.clone(), if missed { "missed" } else { "schedule" }, Value::Null);
                }
            }
            Sched::Watch(spec) => {
                let ws = k.workspace.clone();
                let spec2 = spec.clone();
                let cur = tokio::task::spawn_blocking(move || watch_scan(&ws, &spec2)).await.unwrap_or_default();
                let j = jobs(k);
                let fire = {
                    let mut w = j.watch.locked();
                    match w.get(&job.id) {
                        Some((s, old)) if s == spec => {
                            let ch = changes(old, &cur);
                            if ch.is_empty() {
                                None
                            } else {
                                w.insert(job.id.clone(), (spec.clone(), cur));
                                Some(ch)
                            }
                        }
                        _ => {
                            // first scan (or a changed spec) is the baseline
                            w.insert(job.id.clone(), (spec.clone(), cur));
                            None
                        }
                    }
                };
                if let Some(ch) = fire {
                    spawn_run(k, job.id.clone(), "watch", json!({ "changed": ch }));
                }
            }
            Sched::Webhook => {}
        }
    }
}

async fn run_loop(w: std::sync::Weak<Kernel>) {
    let every = tick();
    let mut iv = tokio::time::interval(every);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut mono_prev, mut wall_prev) = (Instant::now(), SystemTime::now());
    loop {
        iv.tick().await;
        let Some(k) = w.upgrade() else { return };
        if k.is_shutdown() {
            return;
        }
        let (mono, wall) = (Instant::now(), SystemTime::now());
        let s = slept(wall_prev, wall, mono.duration_since(mono_prev));
        if s > Duration::from_secs(30) {
            k.research.log("kernel", Kind::JobsWake, json!({ "slept_s": s.as_secs() }));
            channels::publish(&k, "kernel/jobs", ChannelData::Json(json!({ "op": "wake", "slept_s": s.as_secs() })));
        }
        (mono_prev, wall_prev) = (mono, wall);
        tick_once(&k).await;
    }
}

// ---------- actions ----------

fn module_of(k: &Kernel, p: &Value, c: &Caller) -> Result<String, String> {
    let from_id = arg(p, "id").and_then(|i| i.split_once('/')).map(|(m, _)| m);
    let m = match (c.module(), arg(p, "module").or(from_id)) {
        (Some(own), Some(m)) if own != m => return Err(format!("{} may only manage its own jobs (not \"{m}\")", c.id)),
        (Some(own), _) => own.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => return Err("\"module\" is required".into()),
    };
    if !valid_module_id(&m) || !k.lock().modules.contains_key(&m) {
        return Err(format!("unknown module \"{m}\""));
    }
    Ok(m)
}

fn job_id(k: &Kernel, p: &Value, c: &Caller) -> Result<(String, String), String> {
    let m = module_of(k, p, c)?;
    let name = arg(p, "name").map(String::from).or_else(|| arg(p, "id").and_then(|i| i.split_once('/')).map(|(_, n)| n.to_string())).ok_or("\"name\" is required")?;
    Ok((format!("{m}/{name}"), m))
}

async fn act_create(k: Arc<Kernel>, p: Value, c: Caller, update: bool) -> Result<Value, String> {
    let (id, module) = job_id(&k, &p, &c)?;
    let name = id.split_once('/').map(|(_, n)| n.to_string()).unwrap_or_default();
    if all_jobs(&k).iter().any(|j| j.id == id && j.source == "manifest") {
        return Err(format!("job {id} is declared in openapp.json; edit the manifest instead"));
    }
    let j = jobs(&k);
    let mut spec = {
        let rt = j.runtime.locked();
        match (rt.get(&id), update) {
            (Some(v), true) => v.clone(),
            (None, true) => return Err(format!("no runtime job {id} (job.create it first)")),
            (Some(_), false) if p.get("replace").and_then(Value::as_bool) != Some(true) => return Err(format!("job {id} exists (job.update, or replace: true)")),
            _ => json!({ "module": module, "name": name, "enabled": true }),
        }
    };
    for key in ["schedule", "action", "enabled"] {
        if let Some(v) = p.get(key) {
            spec[key] = v.clone();
        }
    }
    let job = parse_job(&module, &spec, "runtime")?;
    j.runtime.locked().insert(id.clone(), spec);
    if !update {
        patch_state(&k, &id, |s| {
            s.insert("created".into(), json!(Local::now().to_rfc3339()));
        });
    }
    jobs(&k).watch.locked().remove(&id);
    save(&k);
    k.research.log(&c.id, Kind::Job, json!({ "op": if update { "update" } else { "create" }, "job": id, "kind": job.sched.label() }));
    super::background::refresh(&k);
    k.emit_snapshot();
    Ok(json!({ "ok": true, "job": list(&k).into_iter().find(|x| x["id"] == id.as_str()) }))
}

async fn act_delete(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let (id, _) = job_id(&k, &p, &c)?;
    let removed = jobs(&k).runtime.locked().remove(&id).is_some();
    if !removed {
        return Err(if all_jobs(&k).iter().any(|j| j.id == id) { format!("job {id} is declared in openapp.json; remove it there") } else { format!("no job {id}") });
    }
    jobs(&k).st.locked().remove(&id);
    save(&k);
    k.research.log(&c.id, Kind::Job, json!({ "op": "delete", "job": id }));
    super::background::refresh(&k);
    k.emit_snapshot();
    Ok(json!({ "ok": true, "deleted": id }))
}

async fn act_run_now(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let (id, _) = job_id(&k, &p, &c)?;
    let input = p.get("input").cloned().unwrap_or(Value::Null);
    if p.get("wait").and_then(Value::as_bool) == Some(false) {
        find(&k, &id).ok_or_else(|| format!("no job \"{id}\""))?;
        spawn_run(&k, id.clone(), "manual", input);
        return Ok(json!({ "ok": true, "job": id, "started": true }));
    }
    run_job(&k, &id, "manual", input).await
}

// ---------- webhook route ----------

async fn hook(
    axum::extract::State(k): axum::extract::State<Arc<Kernel>>,
    axum::extract::Path(token): axum::extract::Path<String>,
    method: axum::http::Method,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let id = jobs(&k).st.locked().iter().find(|(_, v)| v["webhook"] == token.as_str()).map(|(id, _)| id.clone());
    let Some(id) = id.filter(|id| find(&k, id).is_some_and(|j| matches!(j.sched, Sched::Webhook) && j.enabled)) else {
        return (StatusCode::NOT_FOUND, axum::Json(json!({ "error": "unknown webhook" }))).into_response();
    };
    if body.len() > 1024 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, axum::Json(json!({ "error": "body larger than 1 MB" }))).into_response();
    }
    let ct = headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body_v = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body)));
    let input = json!({ "method": method.as_str(), "query": query, "content_type": ct, "body": body_v });
    match tokio::time::timeout(Duration::from_secs(25), run_job(&k, &id, "webhook", input)).await {
        Ok(Ok(r)) => axum::Json(r).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, axum::Json(json!({ "error": e }))).into_response(),
        Err(_) => (StatusCode::ACCEPTED, axum::Json(json!({ "ok": true, "job": id, "note": "still running (see kernel/jobs)" }))).into_response(),
    }
}

fn router(k: Arc<Kernel>) -> axum::Router {
    axum::Router::new().route("/hook/{token}", axum::routing::any(hook)).with_state(k)
}

fn on_event(k: &Kernel, ev: &Event) {
    if let Event::Started = ev
        && !jobs(k).started.swap(true, Ordering::SeqCst)
        && let Some(ka) = k.arc()
    {
        k.spawn(run_loop(Arc::downgrade(&ka)));
    }
}

pub fn primitive() -> Primitive {
    let create = handler(|k, p, c| async move { act_create(k, p, c, false).await });
    let update = handler(|k, p, c| async move { act_create(k, p, c, true).await });
    let delete = handler(act_delete);
    let run_now = handler(act_run_now);
    let listh = handler(|k, _p, _c| async move { Ok::<_, String>(json!({ "jobs": list(&k) })) });
    Primitive {
        name: "jobs",
        purpose: "Scheduled and triggered work: cron, every N, file watch and local webhooks run a module tool, host action or agent; runs are logged, visible (host data jobs, channel kernel/jobs) and missed runs run once on wake.",
        example: r#"openapp.json: "jobs": [{ "name": "morning", "schedule": "0 8 * * *", "action": { "agent": "summary" } }, { "name": "sync", "schedule": { "every": "15m" }, "action": { "tool": "sync" } }, { "name": "inbox", "schedule": { "watch": "inbox/*.eml" }, "action": { "tool": "import" } }, { "name": "hook", "schedule": { "webhook": true }, "action": { "tool": "on_hook" } }] · spec: { "type": "list", "items": { "$host": "jobs" } }"#,
        class: Class::Free,
        actions: vec![
            Action { name: "job.create", args: "{ module, name, schedule: \"<cron>\" | {every} | {watch} | {webhook: true}, action?: {tool, args?} | {host, …} | {agent, input?}, replace? } → { job (url for webhooks) }", handler: create.clone() },
            Action { name: "job.update", args: "{ module, name, schedule?, action?, enabled? } (runtime jobs)", handler: update.clone() },
            Action { name: "job.delete", args: "{ module, name } (runtime jobs)", handler: delete.clone() },
            Action { name: "job.run_now", args: "{ id: \"<module>/<name>\" | module + name, input?, wait? (default true) } → { status, output_tail, ms }", handler: run_now.clone() },
            Action { name: "job.list", args: "{} → { jobs }", handler: listh.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "job.create", args: "{ name, schedule, action? } (the calling module's job)", handler: create },
            Endpoint { method: "job.update", args: "{ name, schedule?, action?, enabled? }", handler: update },
            Endpoint { method: "job.delete", args: "{ name }", handler: delete },
            Endpoint { method: "job.run_now", args: "{ name, input?, wait? }", handler: run_now },
            Endpoint { method: "job.list", args: "{}", handler: listh },
        ],
        data: vec![DataSource {
            name: "jobs",
            doc: "list items [{ id: \"<module>/<name>\", title, subtitle: \"module · every 15m · last ok 3 min ago · next 08:00\", icon, status: scheduled|running|disabled, last_status, next, url? }] — jobs (Run now: { host: \"job.run_now\", id })",
            get: data_fn(host_items),
        }],
        router: Some(router),
        on_event: Some(Arc::new(on_event)),
        describe_keys: vec![(
            "jobs",
            data_fn(|k| {
                let l = list(k);
                let problems: Vec<String> = jobs(k).problems.locked().values().flatten().cloned().collect();
                let recent: Vec<Value> = jobs(k).recent.locked().iter().rev().take(10).cloned().collect();
                json!({ "jobs": l, "problems": problems, "recent_runs": recent, "tick_ms": tick().as_millis() as u64,
                    "doc": "cron = local time, 5 fields (\"0 8 * * *\"); missed runs (sleep, app closed) run once on wake; runs publish {op: start|end, job, run, status, output_tail} on kernel/jobs" })
            }),
        )],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<Local> {
        Local.from_local_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()).unwrap()
    }

    #[test]
    fn schedules_and_missed_runs() {
        assert_eq!(parse_every("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_every("1h30m").unwrap(), Duration::from_secs(5400));
        assert!(parse_every("15").is_err() && parse_every("0s").is_err() && parse_every("5x").is_err());
        let cron = parse_sched(&json!("0 8 * * *")).unwrap();
        // on time: a tick at 08:00:01 after a run yesterday
        assert_eq!(due(&cron, t("2026-10-16 08:00:00"), t("2026-10-17 07:59:59")), None);
        assert_eq!(due(&cron, t("2026-10-16 08:00:00"), t("2026-10-17 08:00:01")), Some(false));
        // asleep from 07:00 to 11:30: overdue → one missed run; after it (anchor = run time) nothing until tomorrow
        assert_eq!(due(&cron, t("2026-10-16 08:00:00"), t("2026-10-17 11:30:00")), Some(true));
        assert_eq!(due(&cron, t("2026-10-17 11:30:00"), t("2026-10-17 11:30:05")), None);
        // closed for three days: still a single run, then the normal schedule
        assert_eq!(due(&cron, t("2026-10-13 08:00:00"), t("2026-10-17 09:00:00")), Some(true));
        assert_eq!(next_after(&cron, t("2026-10-17 09:00:00")), Some(t("2026-10-18 08:00:00")));
        let every = parse_sched(&json!({ "every": "15m" })).unwrap();
        assert_eq!(due(&every, t("2026-10-17 10:00:00"), t("2026-10-17 10:14:59")), None);
        assert_eq!(due(&every, t("2026-10-17 10:00:00"), t("2026-10-17 10:15:00")), Some(false));
        assert_eq!(due(&every, t("2026-10-17 10:00:00"), t("2026-10-17 13:00:00")), Some(true));
        let secs = parse_sched(&json!("*/2 * * * * *")).unwrap();
        assert!(due(&secs, t("2026-10-17 10:00:00"), t("2026-10-17 10:00:02")) == Some(false));
        assert!(parse_sched(&json!("0 8 * *")).is_err() && parse_sched(&json!({ "watch": "../x" })).is_err());
        assert!(matches!(parse_sched(&json!({ "webhook": true })).unwrap(), Sched::Webhook));
        // sleep detection: wall clock moved 2 h, monotonic 1 s
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(slept(w0, w0 + Duration::from_secs(7201), Duration::from_secs(1)), Duration::from_secs(7200));
        assert_eq!(slept(w0, w0 + Duration::from_secs(1), Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn globs() {
        assert!(glob_match("notes/**/*.md", "notes/a.md") && glob_match("notes/**/*.md", "notes/x/y/b.md"));
        assert!(!glob_match("notes/**/*.md", "notes/a.txt") && !glob_match("notes/*.md", "notes/x/a.md"));
        assert!(glob_match("in?.txt", "in1.txt") && glob_match("data/**", "data/a/b"));
    }
}
