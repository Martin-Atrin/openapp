//! G9 lifecycle, budgets, safe mode (contract §21).
//! - Lazy activation: manifest `"activation": ["onStart" | "onView" | "onAction" | "onJob" | "onChannel"]`
//!   (default onView + onAction). At kernel start a module that is not activated yet is loaded "idle": views
//!   and default placements are read, the server and host part do not start. A visible placement (onView),
//!   a tool action (always), or a subscriber to its topics (onChannel) starts it.
//! - Idle stop: a running module server without visible views, actions, view input or outside subscribers
//!   for `OPENAPP_IDLE_SECS` (default 600) goes back to "idle". Modules with a `"restart": "always"` service stay.
//! - Resources: every `OPENAPP_SAMPLE_MS` (default 5000) CPU % and RSS per module (host: process tree of the
//!   server and host part; VM: processes with OPENAPP_MODULE=<id> read from /proc in the guest), disk every 30 s
//!   → host data `resources`, describe `resources`; budgets (manifest `"budget": { "cpu", "rss_mb" }`) warn.
//! - Safe mode: `/safe` (host action `safe.set {on}`) stops all modules and hides the layout; `/safe off`. Persisted.

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_ACTIVATION: &[&str] = &["onView", "onAction"];
const DEFAULT_CPU_BUDGET: f64 = 90.0;
const DEFAULT_RSS_BUDGET_MB: f64 = 2048.0;
const OVER_SAMPLES: u32 = 3;
const DISK_EVERY: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Life {
    last: Mutex<HashMap<String, Instant>>,
    activating: Mutex<HashSet<String>>,
    resources: Mutex<BTreeMap<String, Value>>,
    vm_prev: Mutex<HashMap<String, (u64, Instant)>>,
    disk: Mutex<HashMap<String, (f64, Instant)>>,
    over: Mutex<HashMap<String, u32>>,
    warned: Mutex<HashSet<String>>,
    safe: Mutex<Option<bool>>,
    started: AtomicBool,
}

fn life(k: &Kernel) -> Arc<Life> {
    k.prims.get::<Life>()
}

pub fn idle_timeout() -> Duration {
    std::env::var("OPENAPP_IDLE_SECS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|s| *s > 0.0).map(Duration::from_secs_f64).unwrap_or(Duration::from_secs(600))
}

pub fn sample_every() -> Duration {
    std::env::var("OPENAPP_SAMPLE_MS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|s| *s >= 100).map(Duration::from_millis).unwrap_or(Duration::from_secs(5))
}

// ---------- safe mode ----------

fn safe_path(k: &Kernel) -> std::path::PathBuf {
    k.state_dir.join("safe_mode.json")
}

pub fn safe_mode(k: &Kernel) -> bool {
    let l = life(k);
    let mut s = l.safe.locked();
    *s.get_or_insert_with(|| {
        std::fs::read(safe_path(k)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["on"].as_bool()).unwrap_or(false)
    })
}

pub async fn set_safe(k: &Arc<Kernel>, on: bool) -> Value {
    if safe_mode(k) == on {
        return json!({ "ok": true, "safe_mode": on, "changed": false });
    }
    *life(k).safe.locked() = Some(on);
    let _ = std::fs::write(safe_path(k), json!({ "on": on, "ts": crate::state::now_ts() }).to_string());
    k.research.log("user", Kind::SafeMode, json!({ "on": on }));
    let ids: Vec<(String, bool)> = k.lock().modules.values().map(|m| (m.id.clone(), m.client.is_some() || m.host.is_some())).collect();
    if on {
        for (id, running) in &ids {
            if *running {
                let _ = k.module_unload(id, false).await;
            }
        }
    } else {
        for (id, _) in &ids {
            autoload(k, id).await;
        }
    }
    k.emit_snapshot();
    json!({ "ok": true, "safe_mode": on, "changed": true })
}

fn slash_safe(k: &Arc<Kernel>, rest: &str) {
    let on = !matches!(rest, "off" | "0" | "false");
    k.chat_append(json!({ "kind": "user", "text": if on { "/safe".to_string() } else { "/safe off".to_string() } }));
    let k = k.clone();
    tokio::spawn(async move {
        set_safe(&k, on).await;
        k.system_message(
            "system",
            if on {
                "Safe mode: no modules run and the layout is hidden. Your layout and modules are kept. Type /safe off to leave safe mode."
            } else {
                "Safe mode is off: modules start again and the layout is back."
            },
        );
    });
}

// ---------- activation ----------

pub fn activation(mf: &crate::modules::Manifest) -> Vec<String> {
    match mf.raw.get("activation").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
        None => DEFAULT_ACTIVATION.iter().map(|s| s.to_string()).collect(),
    }
}

/// A service declared with `"restart": "always"` (G2) or a host-module service (`"host": {"service": true}`, G8)
/// keeps the module running.
fn keeps_running(mf: &crate::modules::Manifest) -> bool {
    mf.raw.get("services").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x["restart"] == "always")) || super::host_service::is_service(mf)
}

/// The module has a placement in the layout or an open presentation.
fn has_view(k: &Kernel, id: &str) -> bool {
    let g = k.lock();
    let prefix = format!("{id}/");
    g.layout.placements.iter().any(|p| p.module == id) || g.presented.iter().any(|p| p["view"].as_str().is_some_and(|v| v.starts_with(&prefix)))
}

fn status(k: &Kernel, id: &str) -> Option<String> {
    k.lock().modules.get(id).map(|m| m.status.clone())
}

fn manifest(k: &Kernel, id: &str) -> Option<crate::modules::Manifest> {
    k.lock().modules.get(id).and_then(|m| m.manifest.clone())
}

pub fn touch(k: &Kernel, module: &str) {
    life(k).last.locked().insert(module.to_string(), Instant::now());
}

/// Kernel start / `/safe off`: load a module eagerly or idle, per its activation.
pub async fn autoload(k: &Arc<Kernel>, id: &str) {
    if safe_mode(k) {
        return;
    }
    let mf = match crate::modules::read_manifest(&k.workspace.join("modules").join(id), id) {
        Ok(m) => m,
        Err(_) => {
            // let module_load report the manifest problems as usual
            let _ = k.module_load(id).await;
            return;
        }
    };
    let act = activation(&mf);
    let has = |a: &str| act.iter().any(|x| x == a);
    let nothing_to_start = mf.server.is_none() && mf.host.is_none();
    if nothing_to_start || has("onStart") || keeps_running(&mf) || (has("onView") && has_view(k, id)) {
        let _ = k.module_load(id).await;
        return;
    }
    let _ = k.module_load_opts(id, false).await;
    // first load: default placements were just applied
    if has("onView") && has_view(k, id) {
        let _ = activate(k, id, "onView").await;
    }
}

/// Start an idle module (no-op when it is not idle).
pub async fn activate(k: &Arc<Kernel>, id: &str, why: &str) -> Result<(), String> {
    if status(k, id).as_deref() != Some("idle") {
        return Ok(());
    }
    let first = life(k).activating.locked().insert(id.to_string());
    if !first {
        // someone else activates it: wait for the result
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if !life(k).activating.locked().contains(id) {
                break;
            }
        }
        return match status(k, id).as_deref() {
            Some("running") => Ok(()),
            s => Err(format!("module \"{id}\" did not start ({})", s.unwrap_or("unknown"))),
        };
    }
    let t0 = Instant::now();
    let r = k.module_load(id).await;
    life(k).activating.locked().remove(id);
    touch(k, id);
    k.research.log(&format!("module:{id}"), Kind::Activate, json!({ "why": why, "ok": r.is_ok(), "ms": t0.elapsed().as_millis() as u64 }));
    r.map(|_| ()).map_err(|e| e["error"].as_str().map(String::from).unwrap_or_else(|| e.to_string()))
}

/// Before a tool call: activate an idle module (onAction) and mark it used.
pub async fn ensure_active(k: &Arc<Kernel>, module: &str) -> Result<(), String> {
    touch(k, module);
    if status(k, module).as_deref() == Some("idle") {
        activate(k, module, "onAction").await?;
    }
    Ok(())
}

/// A subscriber appeared on one of the module's topics.
pub fn on_channel(k: &Kernel, module: &str) {
    touch(k, module);
    if status(k, module).as_deref() == Some("idle")
        && manifest(k, module).is_some_and(|mf| activation(&mf).iter().any(|a| a == "onChannel"))
        && let Some(ka) = k.arc()
    {
        let id = module.to_string();
        k.spawn(async move {
            let _ = activate(&ka, &id, "onChannel").await;
        });
    }
}

/// onView: idle modules that now have a visible placement.
fn on_snapshot(k: &Kernel, s: &crate::state::Snapshot) {
    let idle: Vec<&str> = s.modules.iter().filter(|m| m.status == "idle").map(|m| m.id.as_str()).collect();
    if idle.is_empty() {
        return;
    }
    for id in idle {
        let prefix = format!("{id}/");
        let visible = s.layout.placements.iter().any(|p| p.module == id) || s.presented.iter().any(|p| p["view"].as_str().is_some_and(|v| v.starts_with(&prefix)));
        if !visible || life(k).activating.locked().contains(id) {
            continue;
        }
        if !manifest(k, id).is_some_and(|mf| activation(&mf).iter().any(|a| a == "onView")) {
            continue;
        }
        if let Some(ka) = k.arc() {
            let id = id.to_string();
            k.spawn(async move {
                let _ = activate(&ka, &id, "onView").await;
            });
        }
    }
}

// ---------- idle stop ----------

async fn idle_check(k: &Arc<Kernel>) {
    let timeout = idle_timeout();
    let running: Vec<(String, crate::modules::Manifest)> = k
        .lock()
        .modules
        .values()
        .filter(|m| m.status == "running" && (m.client.is_some() || m.host.is_some()))
        .filter_map(|m| m.manifest.clone().map(|mf| (m.id.clone(), mf)))
        .collect();
    let hub = super::channels::hub(k);
    for (id, mf) in running {
        if keeps_running(&mf) || has_view(k, &id) || life(k).activating.locked().contains(&id) {
            continue;
        }
        if hub.outside_subscribers(&id) > 0 {
            continue;
        }
        let last = life(k).last.locked().get(&id).copied();
        if last.is_some_and(|t| t.elapsed() < timeout) {
            continue;
        }
        if last.is_none() {
            touch(k, &id);
            continue;
        }
        let _ = k.module_unload(&id, false).await;
        if let Some(m) = k.lock().modules.get_mut(&id)
            && m.status == "stopped"
        {
            m.status = "idle".into();
        }
        k.research.log(&format!("module:{id}"), Kind::IdleStop, json!({ "idle_s": timeout.as_secs_f64() }));
        k.emit_snapshot();
    }
}

// ---------- resources ----------

async fn host_ps() -> HashMap<u32, (u32, f64, f64)> {
    let mut out = HashMap::new();
    let Ok(o) = tokio::process::Command::new("ps").args(["-axo", "pid=,ppid=,rss=,pcpu="]).output().await else { return out };
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 4
            && let (Ok(pid), Ok(ppid), Ok(rss), Ok(cpu)) = (f[0].parse(), f[1].parse(), f[2].parse::<f64>(), f[3].replace(',', ".").parse::<f64>())
        {
            out.insert(pid, (ppid, rss, cpu));
        }
    }
    out
}

/// (cpu %, rss KiB, processes) of a process tree.
fn tree_usage(ps: &HashMap<u32, (u32, f64, f64)>, root: u32) -> (f64, f64, usize) {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let (mut cpu, mut rss, mut n) = (0.0, 0.0, 0);
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if let Some((_, r, c)) = ps.get(&p) {
            cpu += c;
            rss += r;
            n += 1;
        }
        stack.extend(ps.iter().filter(|(_, (pp, _, _))| *pp == p).map(|(c, _)| *c));
    }
    (cpu, rss, n)
}

const GUEST_PS: &str = r#"import os
for p in os.listdir('/proc'):
    if not p.isdigit(): continue
    try:
        env = open('/proc/%s/environ' % p, 'rb').read().split(b'\0')
        m = next((e[15:].decode() for e in env if e.startswith(b'OPENAPP_MODULE=')), None)
        if not m: continue
        st = open('/proc/%s/stat' % p).read()
        f = st[st.rindex(')') + 2:].split()
        print(m, int(f[11]) + int(f[12]), int(f[21]))
    except Exception:
        pass
"#;

/// module → (cpu ticks, rss pages, processes) in the guest.
async fn guest_ps(vm: &crate::vm::AppVm) -> HashMap<String, (u64, u64, usize)> {
    let mut out: HashMap<String, (u64, u64, usize)> = HashMap::new();
    let Ok((0, text, _)) = vm.exec_out("python3", &["-c".into(), GUEST_PS.into()], &[], None).await else { return out };
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() == 3
            && let (Ok(t), Ok(r)) = (f[1].parse::<u64>(), f[2].parse::<u64>())
        {
            let e = out.entry(f[0].to_string()).or_default();
            e.0 += t;
            e.1 += r;
            e.2 += 1;
        }
    }
    out
}

async fn disk_mb(dir: &std::path::Path) -> Option<f64> {
    let o = tokio::process::Command::new("du").arg("-sk").arg(dir).output().await.ok()?;
    String::from_utf8_lossy(&o.stdout).split_whitespace().next()?.parse::<f64>().ok().map(|kb| (kb / 1024.0 * 10.0).round() / 10.0)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

async fn sample(k: &Arc<Kernel>) {
    struct M {
        id: String,
        name: String,
        status: String,
        in_vm: bool,
        dir: std::path::PathBuf,
        host_pid: Option<u32>,
        budget: (f64, f64),
    }
    let mods: Vec<M> = {
        let g = k.lock();
        g.modules
            .values()
            .map(|m| {
                let b = m.manifest.as_ref().and_then(|mf| mf.raw.get("budget").cloned()).unwrap_or(Value::Null);
                M {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    status: m.status.clone(),
                    in_vm: m.in_vm && m.client.is_some(),
                    dir: k.workspace.join("modules").join(&m.id),
                    host_pid: m.host.as_ref().and_then(|h| h.pid()),
                    budget: (b["cpu"].as_f64().unwrap_or(DEFAULT_CPU_BUDGET), b["rss_mb"].as_f64().unwrap_or(DEFAULT_RSS_BUDGET_MB)),
                }
            })
            .collect()
    };
    let ps = host_ps().await;
    let guest = match k.vm() {
        Some(vm) if mods.iter().any(|m| m.in_vm) => Some(guest_ps(&vm).await),
        _ => None,
    };
    let l = life(k);
    let now = Instant::now();
    let mut res = BTreeMap::new();
    for m in &mods {
        let running = m.status == "running";
        let (mut cpu, mut rss_kb, mut procs) = (0.0, 0.0, 0usize);
        if running && !m.in_vm {
            let pid = crate::modules::host_pids().locked().get(&m.dir).copied();
            if let Some(pid) = pid.filter(|p| ps.contains_key(p)) {
                let (c, r, n) = tree_usage(&ps, pid);
                cpu += c;
                rss_kb += r;
                procs += n;
            }
        }
        if let Some(pid) = m.host_pid {
            let (c, r, n) = tree_usage(&ps, pid);
            cpu += c;
            rss_kb += r;
            procs += n;
        }
        if running && m.in_vm
            && let Some((ticks, pages, n)) = guest.as_ref().and_then(|g| g.get(&m.id)).copied()
        {
            let mut prev = l.vm_prev.locked();
            if let Some((pt, pi)) = prev.get(&m.id) {
                let dt = now.duration_since(*pi).as_secs_f64().max(0.001);
                cpu += ticks.saturating_sub(*pt) as f64 / 100.0 / dt * 100.0;
            }
            prev.insert(m.id.clone(), (ticks, now));
            rss_kb += pages as f64 * 4.0;
            procs += n;
        }
        let disk = {
            let cached = l.disk.locked().get(&m.id).copied();
            match cached {
                Some((v, t)) if t.elapsed() < DISK_EVERY => Some(v),
                _ => {
                    let v = disk_mb(&m.dir).await;
                    if let Some(v) = v {
                        l.disk.locked().insert(m.id.clone(), (v, Instant::now()));
                    }
                    v
                }
            }
        };
        let rss_mb = round1(rss_kb / 1024.0);
        let cpu = round1(cpu);
        res.insert(
            m.id.clone(),
            json!({ "module": m.id, "name": m.name, "status": m.status, "where": if m.in_vm { "vm" } else { "host" }, "cpu": cpu, "rss_mb": rss_mb, "disk_mb": disk, "processes": procs, "budget": { "cpu": m.budget.0, "rss_mb": m.budget.1 }, "ts": crate::state::now_ts() }),
        );
        // budgets: warn the user (toast) and the agent (UI error in openapp_snapshot) once per episode
        let over = running && (cpu > m.budget.0 || rss_mb > m.budget.1);
        let n = {
            let mut o = l.over.locked();
            let n = o.entry(m.id.clone()).or_default();
            *n = if over { *n + 1 } else { 0 };
            *n
        };
        if n == 0 {
            l.warned.locked().remove(&m.id);
        } else if n >= OVER_SAMPLES && l.warned.locked().insert(m.id.clone()) {
            let msg = format!("{} uses a lot of resources: CPU {cpu}% (budget {}%), memory {rss_mb} MB (budget {} MB)", m.name, m.budget.0, m.budget.1);
            k.notify(&msg);
            k.push_ui_error("kernel", Some(&m.id), &format!("budget: {msg}"));
            k.research.log(&format!("module:{}", m.id), Kind::Budget, json!({ "cpu": cpu, "rss_mb": rss_mb, "budget": { "cpu": m.budget.0, "rss_mb": m.budget.1 } }));
        }
    }
    *l.resources.locked() = res;
}

async fn run_loop(k: std::sync::Weak<Kernel>) {
    let every = sample_every();
    let mut iv = tokio::time::interval(every);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_res = String::new();
    loop {
        iv.tick().await;
        let Some(k) = k.upgrade() else { return };
        if k.is_shutdown() {
            return;
        }
        if !safe_mode(&k) {
            idle_check(&k).await;
        }
        sample(&k).await;
        // push changed numbers to views bound to $host resources (rounded values only change slowly)
        let now = serde_json::to_string(&*life(&k).resources.locked()).unwrap_or_default();
        if now != last_res {
            last_res = now;
            k.emit_snapshot();
        }
    }
}

pub fn resources(k: &Kernel) -> BTreeMap<String, Value> {
    life(k).resources.locked().clone()
}

fn on_event(k: &Kernel, ev: &Event) {
    match ev {
        Event::Started => {
            if !life(k).started.swap(true, Ordering::Relaxed)
                && let Some(ka) = k.arc()
            {
                k.spawn(run_loop(Arc::downgrade(&ka)));
            }
        }
        Event::ModuleLoaded(id) => touch(k, id),
        Event::Snapshot(s) => on_snapshot(k, s),
        _ => {}
    }
}

fn status_doc(k: &Kernel) -> Value {
    let l = life(k);
    let last = l.last.locked().clone();
    let g = k.lock();
    let mods: Map<String, Value> = g
        .modules
        .values()
        .map(|m| {
            let act = m.manifest.as_ref().map(activation).unwrap_or_default();
            (m.id.clone(), json!({ "status": m.status, "activation": act, "last_activity_s": last.get(&m.id).map(|t| t.elapsed().as_secs()) }))
        })
        .collect();
    drop(g);
    json!({
        "safe_mode": safe_mode(k),
        "idle_stop_after_s": idle_timeout().as_secs_f64(),
        "sample_every_s": sample_every().as_secs_f64(),
        "activation": "openapp.json \"activation\": [\"onStart\" | \"onView\" | \"onAction\" | \"onJob\" | \"onChannel\"] (default [\"onView\", \"onAction\"]). Status \"idle\" = views shown, server not started yet (or stopped after idle); the first action, visible view or subscriber starts it. A module with a \"restart\": \"always\" service never idles.",
        "budget": format!("openapp.json \"budget\": {{ \"cpu\": percent, \"rss_mb\": MB }} (default {DEFAULT_CPU_BUDGET}% / {DEFAULT_RSS_BUDGET_MB} MB); over budget for {OVER_SAMPLES} samples → the user gets a toast and you get a UI error"),
        "modules": mods,
    })
}

pub fn primitive() -> Primitive {
    Primitive {
        name: "lifecycle",
        purpose: "Lazy module activation (onStart/onView/onAction/onJob/onChannel), idle stop after 10 min, per-module CPU/RAM/disk with budgets, and safe mode.",
        example: "openapp.json: { \"activation\": [\"onView\", \"onAction\"], \"budget\": { \"cpu\": 50, \"rss_mb\": 512 } } · spec: { \"type\": \"list\", \"items\": { \"$host\": \"resources\" } } · chat: /safe, /safe off",
        class: Class::Free,
        actions: vec![Action {
            name: "safe.set",
            args: "{ on: bool } — safe mode: no modules, layout hidden, kernel chrome only (kernel menu item)",
            handler: handler(|k, p, _c| async move {
                let on = p.get("on").and_then(Value::as_bool).unwrap_or(true);
                Ok::<_, String>(set_safe(&k, on).await)
            }),
        }],
        data: vec![
            DataSource {
                name: "resources",
                doc: "list items [{ id: module, title: name, subtitle: \"CPU 3% · 120 MB · disk 40 MB\", icon: \"cpu\", cpu, rss_mb, disk_mb, status }] — per-module usage, sampled every 5 s",
                get: data_fn(|k| {
                    Value::Array(
                        resources(k)
                            .values()
                            .map(|r| {
                                let disk = r["disk_mb"].as_f64().map(|d| format!(" · disk {d} MB")).unwrap_or_default();
                                json!({ "id": r["module"], "title": r["name"], "subtitle": format!("{} · CPU {}% · {} MB{disk}", r["status"].as_str().unwrap_or(""), r["cpu"], r["rss_mb"]), "icon": "cpu", "cpu": r["cpu"], "rss_mb": r["rss_mb"], "disk_mb": r["disk_mb"], "status": r["status"] })
                            })
                            .collect(),
                    )
                }),
            },
            DataSource { name: "safe_mode", doc: "boolean — safe mode is on", get: data_fn(|k| json!(safe_mode(k))) },
        ],
        slash: vec![("/safe", Arc::new(slash_safe))],
        on_event: Some(Arc::new(on_event)),
        describe_keys: vec![
            ("resources", data_fn(|k| json!(resources(k)))),
            ("lifecycle", data_fn(status_doc)),
        ],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree() {
        let mut ps = HashMap::new();
        ps.insert(10, (1, 1000.0, 5.0));
        ps.insert(11, (10, 500.0, 2.5));
        ps.insert(12, (11, 100.0, 0.5));
        ps.insert(20, (1, 9999.0, 50.0));
        assert_eq!(tree_usage(&ps, 10), (8.0, 1600.0, 3));
    }
}
