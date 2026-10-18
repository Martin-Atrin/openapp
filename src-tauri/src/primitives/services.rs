//! G2 services (contract §21): module-declared daemons in the app VM, supervised by the kernel.
//!
//! Manifest (`openapp.json`, read from `manifest.raw`):
//! - `"services": [{ name, command: [...], cwd?, env?, restart?: always|on-failure|never (default on-failure),
//!   health?: {tcp: port} | {http: "http://127.0.0.1:…"}, ports?: [{ port, expose?: view|lan }] }]`
//! - `"apt": ["jq", "vendor/tool.deb"]` — Debian packages (or .deb files in the module folder) installed
//!   before the services start, on every VM boot (the rootfs is fresh; lists + .debs are cached on the
//!   /opt/openapp volume, so a reinstall needs no network). apt goes through the egress proxy (actor `apt`).
//! - `"arch": "amd64"` — run the services' x86_64 binaries with qemu-user (slow; static binaries, or
//!   dynamic ones after `dpkg --add-architecture amd64` + the libs).
//! - `"display"` — see display.rs (one more service, `display`).
//!
//! Each service is one exec session in the VM: `setsid -w sh -c 'echo $$ > pidfile; exec "$@"'`, so the
//! kernel can stop the whole process group (TERM, then KILL after 3 s). Output (stdout + stderr) goes to
//! `<workspace>/.openapp/logs/<module>.<name>.log` and a tail. Restart with backoff (1 s … 30 s; reset
//! after 30 s up). Health checks every 2 s inside the VM (`/dev/tcp` or curl); a service that was healthy
//! and fails 5 checks in a row is restarted. Services start on ModuleLoaded (unchanged specs keep running
//! across reloads) and stop on ModuleUnloaded. Status: host data `services`, describe `services`.

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use crate::modules::{Tail, push_tail, safe_rel, tail_lines, valid_module_id};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Notify;

pub const PID_DIR: &str = "/run/openapp/svc";
const HEALTH_EVERY: Duration = Duration::from_secs(2);
const UNHEALTHY_RESTART: u32 = 5;
const LOG_MAX: u64 = 5 * 1024 * 1024;

// ---------- manifest ----------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Tcp(u16),
    Http(String),
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Expose {
    View,
    Lan,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PortSpec {
    pub port: u16,
    pub expose: Expose,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ServiceSpec {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub restart: Restart,
    pub health: Option<Health>,
    pub ports: Vec<PortSpec>,
    /// `"net": "socks"`: run under proxychains4 (all TCP through the kernel SOCKS5 egress)
    pub socks: bool,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Extras {
    pub services: Vec<ServiceSpec>,
    pub display: Option<super::display::DisplaySpec>,
    pub apt: Vec<String>,
    pub amd64: bool,
}

impl Extras {
    pub fn is_empty(&self) -> bool {
        self.services.is_empty() && self.display.is_none() && self.apt.is_empty()
    }
}

fn valid_name(s: &str) -> bool {
    crate::util::valid_slug(s, 40)
}

fn valid_apt(s: &str) -> bool {
    // name[:arch][=version]
    let name = s.split(['=', ':']).next().unwrap_or("");
    name.len() >= 2
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.:=~".contains(c))
}

fn str_array(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) => {
            Some(a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect())
        }
        _ => None,
    }
}

fn port_of(v: &Value) -> Option<u16> {
    v.as_u64().filter(|p| (1..=65535).contains(p)).map(|p| p as u16)
}

/// Parse `services`, `display`, `apt`, `arch` of a manifest (problems are agent-facing).
pub fn parse(raw: &Value) -> Result<Extras, Vec<String>> {
    let mut p = Vec::new();
    let mut x = Extras::default();
    match raw.get("services") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for (i, s) in a.iter().enumerate() {
                let at = format!("services[{i}]");
                let Some(o) = s.as_object() else {
                    p.push(format!("{at} must be an object {{ \"name\", \"command\": [...] }}"));
                    continue;
                };
                let name = o.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !valid_name(&name) {
                    p.push(format!("{at}.name must match [a-z0-9-]+ (got {:?})", o.get("name")));
                    continue;
                }
                if name == "display" && raw.get("display").is_some_and(|d| !d.is_null()) {
                    p.push(format!("{at}.name \"display\" is reserved for the \"display\" service"));
                    continue;
                }
                if x.services.iter().any(|s| s.name == name) {
                    p.push(format!("{at}.name \"{name}\" is used twice"));
                    continue;
                }
                let Some(command) = str_array(o.get("command")) else {
                    p.push(format!("{at}.command must be a non-empty array of strings, e.g. [\"python3\", \"-m\", \"http.server\", \"8080\"] (use [\"sh\", \"-c\", \"…\"] for a shell line)"));
                    continue;
                };
                let cwd = match o.get("cwd") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(c)) if c.starts_with('/') || safe_rel(c) => Some(c.clone()),
                    Some(v) => {
                        p.push(format!("{at}.cwd must be a path relative to the module folder or an absolute guest path, got {v}"));
                        None
                    }
                };
                let mut env = BTreeMap::new();
                match o.get("env") {
                    None | Some(Value::Null) => {}
                    Some(Value::Object(e)) => {
                        for (k, v) in e {
                            match v.as_str() {
                                Some(s) => {
                                    env.insert(k.clone(), s.to_string());
                                }
                                None => p.push(format!("{at}.env.{k} must be a string")),
                            }
                        }
                    }
                    Some(_) => p.push(format!("{at}.env must be an object of strings")),
                }
                let restart = match o.get("restart").and_then(Value::as_str) {
                    None => Restart::OnFailure,
                    Some("always") => Restart::Always,
                    Some("on-failure") => Restart::OnFailure,
                    Some("never") => Restart::Never,
                    Some(r) => {
                        p.push(format!("{at}.restart must be \"always\", \"on-failure\" or \"never\", got \"{r}\""));
                        Restart::OnFailure
                    }
                };
                let health = match o.get("health") {
                    None | Some(Value::Null) => None,
                    Some(h) => {
                        if let Some(t) = h.get("tcp") {
                            match port_of(t) {
                                Some(port) => Some(Health::Tcp(port)),
                                None => {
                                    p.push(format!("{at}.health.tcp must be a port number"));
                                    None
                                }
                            }
                        } else if let Some(u) = h.get("http").and_then(Value::as_str) {
                            if u.starts_with("http://127.0.0.1:") || u.starts_with("http://localhost:") {
                                Some(Health::Http(u.to_string()))
                            } else {
                                p.push(format!("{at}.health.http must be a URL on the VM itself, e.g. \"http://127.0.0.1:8080/health\""));
                                None
                            }
                        } else {
                            p.push(format!("{at}.health must be {{\"tcp\": 1143}} or {{\"http\": \"http://127.0.0.1:8080/health\"}}"));
                            None
                        }
                    }
                };
                let mut ports = Vec::new();
                match o.get("ports") {
                    None | Some(Value::Null) => {}
                    Some(Value::Array(ps)) => {
                        for (j, pv) in ps.iter().enumerate() {
                            let port = pv.get("port").and_then(port_of).or_else(|| port_of(pv));
                            let expose = match pv.get("expose").and_then(Value::as_str) {
                                None | Some("view") => Some(Expose::View),
                                Some("lan") => Some(Expose::Lan),
                                Some(e) => {
                                    p.push(format!("{at}.ports[{j}].expose must be \"view\" or \"lan\", got \"{e}\""));
                                    None
                                }
                            };
                            match (port, expose) {
                                (Some(port), Some(expose)) => ports.push(PortSpec { port, expose }),
                                (None, _) => p.push(format!("{at}.ports[{j}] must be {{ \"port\": 8080, \"expose\": \"view\" }}")),
                                _ => {}
                            }
                        }
                    }
                    Some(_) => p.push(format!("{at}.ports must be an array of {{ port, expose }}")),
                }
                let socks = match o.get("net").and_then(Value::as_str) {
                    None => false,
                    Some("socks") => true,
                    Some(n) => {
                        p.push(format!("{at}.net must be \"socks\" (all TCP through OpenApp's SOCKS proxy, for programs without proxy support) or omitted, got \"{n}\""));
                        false
                    }
                };
                x.services.push(ServiceSpec { name, command, cwd, env, restart, health, ports, socks });
            }
        }
        Some(_) => p.push("\"services\" must be an array of { name, command, … }".into()),
    }
    match raw.get("apt") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for v in a {
                match v.as_str() {
                    Some(s) if s.ends_with(".deb") && safe_rel(s) => x.apt.push(s.to_string()),
                    Some(s) if valid_apt(s) => x.apt.push(s.to_string()),
                    _ => p.push(format!("apt: {v} is not a Debian package name (e.g. \"jq\") or a .deb file in the module folder")),
                }
            }
        }
        Some(_) => p.push("\"apt\" must be an array of package names".into()),
    }
    match raw.get("arch").and_then(Value::as_str) {
        None | Some("arm64") | Some("native") => {}
        Some("amd64") | Some("x86_64") => x.amd64 = true,
        Some(a) => p.push(format!("\"arch\" must be \"amd64\" (x86_64 under qemu-user) or omitted, got \"{a}\"")),
    }
    if x.amd64 && x.services.iter().any(|s| s.socks) {
        p.push("\"net\": \"socks\" does not reach into qemu-user (\"arch\": \"amd64\"): give the program a SOCKS proxy option ($OPENAPP_SOCKS_URL) instead".into());
    }
    match super::display::parse(raw.get("display")) {
        Ok(d) => x.display = d,
        Err(e) => p.extend(e),
    }
    if p.is_empty() { Ok(x) } else { Err(p) }
}

// ---------- runtime state ----------

#[derive(Debug, Clone, Default)]
struct SvcStatus {
    state: &'static str,
    healthy: Option<bool>,
    restarts: u32,
    last_exit: Option<i32>,
    since: Option<String>,
    error: Option<String>,
}

pub struct Svc {
    pub module: String,
    pub spec: ServiceSpec,
    st: Mutex<SvcStatus>,
    stopping: AtomicBool,
    restart_now: AtomicBool,
    wake: Notify,
    tail: Tail,
    done: Notify,
    finished: AtomicBool,
}

impl Svc {
    fn new(module: &str, spec: ServiceSpec) -> Arc<Self> {
        Arc::new(Self {
            module: module.into(),
            spec,
            st: Mutex::new(SvcStatus { state: "starting", ..Default::default() }),
            stopping: AtomicBool::new(false),
            restart_now: AtomicBool::new(false),
            wake: Notify::new(),
            tail: Arc::new(Mutex::new(VecDeque::new())),
            done: Notify::new(),
            finished: AtomicBool::new(false),
        })
    }
    fn set(&self, f: impl FnOnce(&mut SvcStatus)) {
        f(&mut self.st.locked());
    }
    fn pidfile(&self) -> String {
        format!("{PID_DIR}/{}.{}.pid", self.module, self.spec.name)
    }
    pub fn status(&self, k: &Kernel) -> Value {
        let s = self.st.locked().clone();
        let ports: Vec<Value> = self
            .spec
            .ports
            .iter()
            .map(|p| {
                let mut o = json!({ "port": p.port, "expose": p.expose, "url": super::ports::oasvc_url(&self.module, p.port, "/") });
                if p.expose == Expose::Lan {
                    o["lan"] = super::ports::lan_status(k, &self.module, p.port);
                }
                o
            })
            .collect();
        json!({
            "id": format!("{}/{}", self.module, self.spec.name),
            "module": self.module,
            "name": self.spec.name,
            "state": s.state,
            "healthy": s.healthy,
            "restarts": s.restarts,
            "last_exit": s.last_exit,
            "since": s.since,
            "error": s.error,
            "command": self.spec.command,
            "restart": self.spec.restart,
            "health": self.spec.health,
            "ports": ports,
            "log": format!(".openapp/logs/{}.{}.log", self.module, self.spec.name),
        })
    }
}

pub struct ModRun {
    pub extras: Extras,
    pub svcs: Vec<Arc<Svc>>,
    pub problems: Vec<String>,
    /// apt step: "" (none) | "installing" | "ok" | "failed: …"
    pub apt: String,
    pub epoch: u64,
}

#[derive(Default)]
pub struct G2 {
    pub mods: Mutex<BTreeMap<String, ModRun>>,
    apt_lock: tokio::sync::Mutex<()>,
    epoch: AtomicU64,
    /// module → display number (stable for the kernel's lifetime)
    pub displays: Mutex<BTreeMap<String, u16>>,
    /// one start/stop at a time per module (load events can arrive twice in a row)
    locks: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn module_lock(k: &Kernel, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    state(k).locks.locked().entry(id.to_string()).or_default().clone()
}

pub fn state(k: &Kernel) -> Arc<G2> {
    k.prims.get::<G2>()
}

/// Services of one module (or all), as status JSON.
pub fn statuses(k: &Kernel, module: Option<&str>) -> Vec<Value> {
    let st = state(k);
    let mods = st.mods.locked();
    let mut out = Vec::new();
    for (id, m) in mods.iter() {
        if module.is_some_and(|x| x != id) {
            continue;
        }
        for s in &m.svcs {
            out.push(s.status(k));
        }
    }
    out
}

/// All declared ports of a module that a view may reach (expose view or lan).
pub fn module_ports(k: &Kernel, module: &str) -> Vec<u16> {
    let st = state(k);
    let mods = st.mods.locked();
    mods.get(module).map(|m| m.svcs.iter().flat_map(|s| s.spec.ports.iter().map(|p| p.port)).collect()).unwrap_or_default()
}

fn find(k: &Kernel, module: &str, name: &str) -> Option<Arc<Svc>> {
    let st = state(k);
    let mods = st.mods.locked();
    mods.get(module).and_then(|m| m.svcs.iter().find(|s| s.spec.name == name).cloned())
}

fn host_log(k: &Kernel, module: &str, name: &str) -> std::path::PathBuf {
    k.workspace.join(".openapp").join("logs").join(format!("{module}.{name}.log"))
}

async fn append_log(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = tokio::fs::OpenOptions::new().create(true).append(true).open(path).await {
        let _ = f.write_all(line.as_bytes()).await;
        let _ = f.write_all(b"\n").await;
    }
}

// ---------- lifecycle ----------

/// ModuleLoaded: (re)start the module's services. Unchanged specs keep running.
pub async fn start_module(k: Arc<Kernel>, id: String) {
    let lock = module_lock(&k, &id);
    let _g = lock.lock().await;
    let raw = k.lock().modules.get(&id).and_then(|m| m.manifest.as_ref().map(|mf| mf.raw.clone()));
    let Some(raw) = raw else { return };
    let st = state(&k);
    let extras = match parse(&raw) {
        Ok(x) => x,
        Err(problems) => {
            let old = st.mods.locked().remove(&id);
            if let Some(old) = old {
                stop_svcs(&k, old.svcs).await;
            }
            k.research.log(&format!("module:{id}"), Kind::ServicesError, json!({ "problems": problems }));
            let epoch = st.epoch.fetch_add(1, Ordering::Relaxed) + 1;
            st.mods.locked().insert(id.clone(), ModRun { extras: Extras::default(), svcs: vec![], problems, apt: String::new(), epoch });
            k.emit_snapshot();
            return;
        }
    };
    {
        let mods = st.mods.locked();
        if let Some(m) = mods.get(&id)
            && m.extras == extras
            && m.problems.is_empty()
            && !m.svcs.iter().any(|s| s.finished.load(Ordering::Relaxed) || s.stopping.load(Ordering::Relaxed))
        {
            return; // same declaration, still supervised
        }
    }
    let old = st.mods.locked().remove(&id);
    if let Some(old) = old {
        stop_svcs(&k, old.svcs).await;
        super::ports::lan_stop(&k, &id);
    }
    if extras.is_empty() {
        k.emit_snapshot();
        return;
    }
    let epoch = st.epoch.fetch_add(1, Ordering::Relaxed) + 1;
    let mut specs = extras.services.clone();
    if let Some(d) = &extras.display {
        specs.push(super::display::service_spec(&k, &id, d));
    }
    let svcs: Vec<Arc<Svc>> = specs.into_iter().map(|s| Svc::new(&id, s)).collect();
    st.mods.locked().insert(
        id.clone(),
        ModRun { extras: extras.clone(), svcs: svcs.clone(), problems: vec![], apt: if extras.apt.is_empty() { String::new() } else { "waiting".into() }, epoch },
    );
    k.emit_snapshot();
    let vm = match k.ensure_vm().await {
        Ok(v) => v,
        Err(e) => {
            for s in &svcs {
                s.set(|x| {
                    x.state = "error";
                    x.error = Some(format!("the app VM is not running: {e}"));
                });
            }
            k.emit_snapshot();
            return;
        }
    };
    if !extras.apt.is_empty() {
        set_apt(&k, &id, epoch, "installing");
        let r = apt_install(&k, &vm, &id, &extras.apt).await;
        let msg = match &r {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("failed: {e}"),
        };
        set_apt(&k, &id, epoch, &msg);
        if r.is_err() {
            for s in &svcs {
                s.set(|x| {
                    x.state = "error";
                    x.error = Some(format!("apt install {msg} (see .openapp/logs/{id}.apt.log)"));
                });
            }
            k.emit_snapshot();
            return;
        }
    }
    for s in &svcs {
        k.spawn(supervise(Arc::downgrade(&k), s.clone(), extras.amd64 && s.spec.name != "display"));
    }
    for s in &svcs {
        for p in s.spec.ports.iter().filter(|p| p.expose == Expose::Lan) {
            super::ports::lan_start(&k, &id, &s.spec.name, p.port);
        }
    }
    k.research.log(&format!("module:{id}"), Kind::ServicesStart, json!({ "services": svcs.iter().map(|s| s.spec.name.clone()).collect::<Vec<_>>() }));
}

fn set_apt(k: &Kernel, id: &str, epoch: u64, v: &str) {
    let st = state(k);
    if let Some(m) = st.mods.locked().get_mut(id)
        && m.epoch == epoch
    {
        m.apt = v.to_string();
    }
    k.emit_snapshot();
}

/// ModuleUnloaded: stop the module's services.
pub async fn stop_module(k: Arc<Kernel>, id: String) {
    let lock = module_lock(&k, &id);
    let _g = lock.lock().await;
    super::ports::lan_stop(&k, &id);
    super::pty::close_module(&k, &id);
    let (svcs, epoch) = match state(&k).mods.locked().get(&id) {
        Some(m) => (m.svcs.clone(), m.epoch),
        None => return,
    };
    for s in &svcs {
        s.set(|x| x.state = "stopping");
    }
    let n = svcs.len();
    stop_svcs(&k, svcs).await;
    // a reload that started meanwhile owns the entry now
    let st = state(&k);
    let mut mods = st.mods.locked();
    let old = if mods.get(&id).is_some_and(|m| m.epoch == epoch) { mods.remove(&id) } else { None };
    drop(mods);
    if old.is_some() {
        if n > 0 {
            k.research.log(&format!("module:{id}"), Kind::ServicesStop, json!({ "count": n }));
        }
        k.emit_snapshot();
    }
}

async fn stop_svcs(k: &Arc<Kernel>, svcs: Vec<Arc<Svc>>) {
    let vm = k.vm();
    let mut waits = Vec::new();
    for s in svcs {
        s.stopping.store(true, Ordering::Relaxed);
        s.wake.notify_waiters();
        if let Some(vm) = &vm {
            let _ = kill_group(vm, &s.pidfile(), false).await;
        }
        waits.push(s);
    }
    for s in waits {
        let n = s.done.notified();
        if !s.finished.load(Ordering::Relaxed) {
            let _ = tokio::time::timeout(Duration::from_secs(5), n).await;
        }
    }
}

/// TERM the service's process group, KILL after 3 s (or at once with `hard`).
async fn kill_group(vm: &crate::vm::AppVm, pidfile: &str, hard: bool) -> Result<(), String> {
    let script = if hard {
        format!("p=$(cat {pidfile} 2>/dev/null) && [ -n \"$p\" ] && kill -KILL -- -$p 2>/dev/null; rm -f {pidfile}; true")
    } else {
        format!(
            "p=$(cat {pidfile} 2>/dev/null) || exit 0; [ -n \"$p\" ] || exit 0; kill -TERM -- -$p 2>/dev/null || exit 0; \
             for i in $(seq 30); do kill -0 -- -$p 2>/dev/null || exit 0; sleep 0.1; done; kill -KILL -- -$p 2>/dev/null; true"
        )
    };
    // bash: dash's kill builtin rejects `--` before a negative pid
    vm.exec_out("bash", &["-c".into(), script], &[], None).await.map(|_| ())
}

/// Install the module's apt packages (serialized; `apt-get update` when the lists are missing or old).
async fn apt_install(k: &Arc<Kernel>, vm: &crate::vm::AppVm, id: &str, pkgs: &[String]) -> Result<(), String> {
    let st = state(k);
    let _g = st.apt_lock.lock().await;
    let gdir = format!("{}/modules/{id}", crate::vm::GUEST_WS);
    let args: Vec<String> = pkgs.iter().map(|p| if p.ends_with(".deb") { format!("./{p}") } else { p.clone() }).collect();
    let quoted = args.iter().map(|a| format!("'{}'", a.replace('\'', ""))).collect::<Vec<_>>().join(" ");
    // fresh lists at most every 6 h; a failed install retries once after an update
    let script = format!(
        "set -e; cd {gdir}; mkdir -p /opt/openapp/apt/lists/partial /opt/openapp/apt/archives/partial; \
         if [ -z \"$(find /opt/openapp/apt/lists -maxdepth 1 -name '*_Packages*' -mmin -360 2>/dev/null | head -1)\" ]; then apt-get update -q; fi; \
         apt-get install -y -q {quoted} || {{ apt-get update -q && apt-get install -y -q {quoted}; }}"
    );
    let t = Instant::now();
    let log = host_log(k, id, "apt");
    let _ = std::fs::create_dir_all(log.parent().unwrap_or(&k.workspace));
    append_log(&log, &format!("=== {} apt-get install {} ===", crate::state::now_ts(), args.join(" "))).await;
    let (code, out, err) = vm.sh(&script, &[("DEBIAN_FRONTEND".into(), "noninteractive".into())]).await?;
    append_log(&log, out.trim_end()).await;
    append_log(&log, err.trim_end()).await;
    k.research.log(&format!("module:{id}"), Kind::AptInstall, json!({ "packages": args, "ok": code == 0, "ms": t.elapsed().as_millis() as u64 }));
    if code != 0 {
        let last: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!("exit {code}: {}", last.join(" | ")));
    }
    Ok(())
}

fn shell_env(k: &Kernel, module: &str, spec: &ServiceSpec) -> Vec<(String, String)> {
    let mut env = crate::vm::proxy_env(&format!("module.{module}"));
    env.push(("OPENAPP_MODULE".into(), module.into()));
    env.push(("OPENAPP_SERVICE".into(), spec.name.clone()));
    env.extend(k.host_env(module, true));
    env.extend(spec.env.iter().map(|(a, b)| (a.clone(), b.clone())));
    env
}

/// The supervisor of one service (runs until the service is stopped or its policy says done).
async fn supervise(k: std::sync::Weak<Kernel>, s: Arc<Svc>, amd64: bool) {
    let mut backoff = Duration::from_secs(1);
    let log = match k.upgrade() {
        Some(k) => host_log(&k, &s.module, &s.spec.name),
        None => return,
    };
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&log).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(&log, log.with_extension("log.1"));
    }
    loop {
        if s.stopping.load(Ordering::Relaxed) {
            break;
        }
        let Some(kk) = k.upgrade() else { break };
        let Some(vm) = kk.vm() else {
            s.set(|x| x.state = "waiting for VM");
            drop(kk);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        let env = shell_env(&kk, &s.module, &s.spec);
        let cwd = match &s.spec.cwd {
            Some(c) if c.starts_with('/') => c.clone(),
            Some(c) => format!("{}/modules/{}/{c}", crate::vm::GUEST_WS, s.module),
            None => format!("{}/modules/{}", crate::vm::GUEST_WS, s.module),
        };
        super::secret_release::deliver_files(&kk, &vm, &s.module).await; // G7 hook (builder D): released file secrets
        drop(kk);
        // own process group + pidfile (pgid) so stop/restart reach every child
        let mut args: Vec<String> = vec![
            "-w".into(),
            "sh".into(),
            "-c".into(),
            if amd64 {
                "echo $$ > \"$0\"; q=$(command -v qemu-x86_64-static || command -v qemu-x86_64); exec \"$q\" \"$@\"".into()
            } else {
                "echo $$ > \"$0\"; exec \"$@\"".into()
            },
            s.pidfile(),
        ];
        if s.spec.socks {
            args.extend(crate::vm::socks_wrap(&format!("module.{}", s.module), &s.spec.command));
        } else {
            args.extend(s.spec.command.iter().cloned());
        }
        let started = Instant::now();
        append_log(&log, &format!("=== {} start: {} ===", crate::state::now_ts(), s.spec.command.join(" "))).await;
        let child = vm.spawn("setsid", &args, &env, Some(&cwd)).await;
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                s.set(|x| {
                    x.state = "error";
                    x.error = Some(e.clone());
                });
                append_log(&log, &format!("=== spawn failed: {e} ===")).await;
                drop(vm);
                if s.spec.restart == Restart::Never {
                    break;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        s.set(|x| {
            x.state = "running";
            x.error = None;
            x.healthy = None;
            x.since = Some(crate::state::now_ts());
        });
        if let Some(kk) = k.upgrade() {
            kk.emit_snapshot();
        }
        let killer = child.killer();
        for pipe in [child.stdout.take(), child.stderr.take()].into_iter().flatten() {
            let (log, tail) = (log.clone(), s.tail.clone());
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(pipe).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    append_log(&log, &l).await;
                    push_tail(&tail, l);
                }
            });
        }
        drop(child.stdin.take());
        let health = s.spec.health.clone().map(|h| {
            let (s2, vm2, k2) = (s.clone(), vm.clone(), k.clone());
            tokio::spawn(async move { health_loop(k2, vm2, s2, h).await })
        });
        let mut exit_rx = match child.take_exit() {
            Some(rx) => rx,
            None => break,
        };
        let lost = || crate::vm::ExitInfo { code: -1, failed: Some("exec session ended".into()) };
        let info = tokio::select! {
            i = &mut exit_rx => Some(i.unwrap_or_else(|_| lost())),
            _ = wait_stop(&s) => None,
        };
        if let Some(h) = health {
            h.abort();
        }
        let info = match info {
            Some(i) => i,
            None => {
                // stopped or restart requested: the group gets TERM (KILL after 3 s); reap the exec session
                let _ = kill_group(&vm, &s.pidfile(), false).await;
                let i = tokio::time::timeout(Duration::from_secs(5), &mut exit_rx).await;
                if i.is_err() {
                    let _ = kill_group(&vm, &s.pidfile(), true).await;
                    killer.kill();
                }
                i.ok().and_then(|r| r.ok()).unwrap_or_else(lost)
            }
        };
        drop(vm);
        let ran = started.elapsed();
        append_log(&log, &format!("=== {} exited: code {}{} after {} s ===", crate::state::now_ts(), info.code, info.failed.as_ref().map(|f| format!(" ({f})")).unwrap_or_default(), ran.as_secs())).await;
        let forced = s.restart_now.swap(false, Ordering::Relaxed);
        if s.stopping.load(Ordering::Relaxed) {
            s.set(|x| {
                x.state = "stopped";
                x.last_exit = Some(info.code);
                x.healthy = None;
            });
            break;
        }
        let again = forced
            || match s.spec.restart {
                Restart::Always => true,
                Restart::OnFailure => info.code != 0,
                Restart::Never => false,
            };
        s.set(|x| {
            x.last_exit = Some(info.code);
            x.healthy = None;
            if let Some(f) = &info.failed {
                x.error = Some(f.clone());
            }
            if again {
                x.restarts += 1;
                x.state = "restarting";
            } else {
                x.state = if info.code == 0 { "exited" } else { "failed" };
            }
        });
        if let Some(kk) = k.upgrade() {
            kk.research.log(&format!("module:{}", s.module), Kind::ServiceExit, json!({ "service": s.spec.name, "code": info.code, "restart": again, "secs": ran.as_secs() }));
            kk.emit_snapshot();
        }
        if !again {
            break;
        }
        if ran > Duration::from_secs(30) || forced {
            backoff = Duration::from_secs(1);
        }
        if !forced {
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {},
                _ = wait_stop(&s) => {},
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
    s.finished.store(true, Ordering::Relaxed);
    s.done.notify_waiters();
    if let Some(kk) = k.upgrade() {
        kk.emit_snapshot();
    }
}

async fn wait_stop(s: &Svc) {
    loop {
        let n = s.wake.notified();
        if s.stopping.load(Ordering::Relaxed) || s.restart_now.load(Ordering::Relaxed) {
            return;
        }
        n.await;
    }
}

async fn health_loop(k: std::sync::Weak<Kernel>, vm: Arc<crate::vm::AppVm>, s: Arc<Svc>, h: Health) {
    let script = match &h {
        Health::Tcp(p) => format!("exec 3<>/dev/tcp/127.0.0.1/{p}"),
        Health::Http(u) => format!("curl -fsS -o /dev/null --max-time 3 --noproxy '*' '{}'", u.replace('\'', "")),
    };
    let mut fails = 0u32;
    let mut was_healthy = false;
    tokio::time::sleep(Duration::from_millis(500)).await;
    loop {
        let ok = matches!(vm.exec_out("bash", &["-c".into(), script.clone()], &[], None).await, Ok((0, _, _)));
        let changed = {
            let mut st = s.st.locked();
            let c = st.healthy != Some(ok);
            st.healthy = Some(ok);
            c
        };
        if changed && let Some(kk) = k.upgrade() {
            kk.emit_snapshot();
        }
        if ok {
            was_healthy = true;
            fails = 0;
        } else {
            fails += 1;
            if was_healthy && fails >= UNHEALTHY_RESTART && !s.stopping.load(Ordering::Relaxed) {
                append_log(&k.upgrade().map(|kk| host_log(&kk, &s.module, &s.spec.name)).unwrap_or_default(), "=== health check failed 5 times: restarting ===").await;
                s.restart_now.store(true, Ordering::Relaxed);
                s.wake.notify_waiters();
                let _ = kill_group(&vm, &s.pidfile(), false).await;
                return;
            }
        }
        tokio::time::sleep(HEALTH_EVERY).await;
    }
}

// ---------- actions ----------

/// Which module a caller may manage: its own (module/view/host callers) or any (agent, user, kernel).
pub fn allowed_module(c: &Caller, module: &str) -> Result<(), String> {
    match c.module() {
        Some(m) if m != module => Err(format!("{} may only manage its own module's services (asked for \"{module}\")", c.id)),
        _ => Ok(()),
    }
}

fn module_arg(p: &Value, c: &Caller) -> Result<String, String> {
    let m = arg(p, "module").or_else(|| c.module()).ok_or("\"module\" is required")?;
    if !valid_module_id(m) {
        return Err(format!("\"{m}\" is not a module id"));
    }
    allowed_module(c, m)?;
    Ok(m.to_string())
}

async fn act_restart(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = module_arg(&p, &c)?;
    let name = arg(&p, "name").ok_or("\"name\" is required")?;
    let s = find(&k, &m, name).ok_or_else(|| format!("module \"{m}\" has no service \"{name}\" (declare it in openapp.json \"services\")"))?;
    if s.finished.load(Ordering::Relaxed) {
        // ended (never / exited): start the module's services again
        state(&k).mods.locked().remove(&m);
        start_module(k.clone(), m.clone()).await;
        return Ok(json!({ "ok": true, "restarted": true }));
    }
    s.restart_now.store(true, Ordering::Relaxed);
    s.wake.notify_waiters();
    if let Some(vm) = k.vm() {
        kill_group(&vm, &s.pidfile(), false).await?;
    }
    k.research.log(&c.id, Kind::ServiceRestart, json!({ "module": m, "service": name }));
    Ok(json!({ "ok": true, "restarted": true }))
}

async fn act_stop(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = module_arg(&p, &c)?;
    let name = arg(&p, "name").ok_or("\"name\" is required")?;
    let s = find(&k, &m, name).ok_or_else(|| format!("module \"{m}\" has no service \"{name}\""))?;
    stop_svcs(&k, vec![s]).await;
    k.research.log(&c.id, Kind::ServiceStop, json!({ "module": m, "service": name }));
    k.emit_snapshot();
    Ok(json!({ "ok": true, "state": "stopped", "start_again": "service.restart" }))
}

async fn act_status(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = match arg(&p, "module") {
        Some(m) => {
            allowed_module(&c, m)?;
            Some(m.to_string())
        }
        None => c.module().map(String::from),
    };
    let st = state(&k);
    let (problems, apt) = {
        let mods = st.mods.locked();
        let mut problems = Map::new();
        let mut apt = Map::new();
        for (id, r) in mods.iter().filter(|(id, _)| m.as_deref().is_none_or(|x| x == id.as_str())) {
            if !r.problems.is_empty() {
                problems.insert(id.clone(), json!(r.problems));
            }
            if !r.apt.is_empty() {
                apt.insert(id.clone(), json!(r.apt));
            }
        }
        (problems, apt)
    };
    Ok(json!({ "services": statuses(&k, m.as_deref()), "problems": problems, "apt": apt }))
}

async fn act_logs(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = module_arg(&p, &c)?;
    let name = arg(&p, "name").ok_or("\"name\" is required")?;
    let n = p.get("lines").and_then(Value::as_u64).unwrap_or(60).min(200) as usize;
    let s = find(&k, &m, name).ok_or_else(|| format!("module \"{m}\" has no service \"{name}\""))?;
    Ok(json!({ "lines": tail_lines(&s.tail, n), "log": format!(".openapp/logs/{m}.{name}.log") }))
}

fn data_services(k: &Kernel) -> Value {
    Value::Array(
        statuses(k, None)
            .into_iter()
            .map(|s| {
                let state = s["state"].as_str().unwrap_or("").to_string();
                let health = match s["healthy"].as_bool() {
                    Some(true) => " · healthy",
                    Some(false) => " · unhealthy",
                    None => "",
                };
                let ports: Vec<String> = s["ports"].as_array().map(|a| a.iter().filter_map(|p| p["port"].as_u64()).map(|p| format!(":{p}")).collect()).unwrap_or_default();
                json!({
                    "id": s["id"],
                    "title": s["name"],
                    "subtitle": format!("{} · {state}{health}{}{}", s["module"].as_str().unwrap_or(""), if ports.is_empty() { String::new() } else { format!(" · {}", ports.join(" ")) }, if s["restarts"].as_u64().unwrap_or(0) > 0 { format!(" · {} restarts", s["restarts"]) } else { String::new() }),
                    "icon": "server",
                    "status": state,
                    "healthy": s["healthy"],
                    "module": s["module"],
                    "name": s["name"],
                    "ports": s["ports"],
                    "action": { "host": "service.restart", "module": s["module"], "name": s["name"] },
                })
            })
            .collect(),
    )
}

pub fn primitive() -> Primitive {
    let actions = vec![
        Action { name: "service.status", args: "{ module? } → { services: [{ id, state, healthy, restarts, ports[{port, url}], log }], problems, apt }", handler: handler(act_status) },
        Action { name: "service.restart", args: "{ module, name } — restart (or start again) a declared service", handler: handler(act_restart) },
        Action { name: "service.stop", args: "{ module, name } — stop until service.restart or the next module load", handler: handler(act_stop) },
        Action { name: "service.logs", args: "{ module, name, lines? } → { lines } (full log: .openapp/logs/<module>.<name>.log)", handler: handler(act_logs) },
        Action { name: "service.url", args: "{ module, port, path? } → { url: oasvc://<module>/<port>/<path>, ws: ws://127.0.0.1:…/… (WebSocket + HTTP gateway) }", handler: handler(super::ports::act_url) },
    ];
    let endpoints = vec![
        Endpoint { method: "service.status", args: "{ module? }", handler: handler(act_status) },
        Endpoint { method: "service.restart", args: "{ name }", handler: handler(act_restart) },
        Endpoint { method: "service.logs", args: "{ name, lines? }", handler: handler(act_logs) },
        Endpoint { method: "service.url", args: "{ port, path? }", handler: handler(super::ports::act_url) },
    ];
    Primitive {
        name: "services",
        purpose: "Long-running daemons in the app VM (manifest \"services\"; \"apt\" packages; \"arch\": \"amd64\" via qemu-user), supervised with restart + health checks, logs in .openapp/logs/<module>.<name>.log; ports shown in views via oasvc://<module>/<port>/ or on the LAN (expose \"lan\", consent)",
        example: "\"services\": [{ \"name\": \"web\", \"command\": [\"python3\", \"-m\", \"http.server\", \"8080\"], \"restart\": \"always\", \"health\": { \"tcp\": 8080 }, \"ports\": [{ \"port\": 8080, \"expose\": \"view\" }] }] → html view <iframe src=\"oasvc://<module>/8080/\">",
        class: Class::Free,
        actions,
        endpoints,
        data: vec![DataSource {
            name: "services",
            doc: "list items [{ id: \"<module>/<name>\", title, subtitle (module · state · health · ports · restarts), icon: \"server\", status, healthy, ports, action: {host:\"service.restart\", module, name} }]",
            get: data_fn(data_services),
        }],
        on_event: Some(Arc::new(|k: &Kernel, ev: &Event| {
            let Some(ka) = k.arc() else { return };
            match ev {
                Event::ModuleLoaded(id) => {
                    let id = id.to_string();
                    k.spawn(start_module(ka, id));
                }
                Event::ModuleUnloaded(id) => {
                    let id = id.to_string();
                    k.spawn(stop_module(ka, id));
                }
                _ => {}
            }
        })),
        describe_keys: vec![(
            "services",
            data_fn(|k| {
                json!({
                    "running": statuses(k, None),
                    "manifest": "\"services\": [{ name, command: [...], cwd?, env?, restart?: always|on-failure|never, health?: {tcp: port}|{http: url}, ports?: [{ port, expose: view|lan }], net?: \"socks\" (proxychains4: all TCP through the SOCKS egress) }], \"apt\": [packages], \"arch\"?: \"amd64\", \"display\"?: { command, width, height }",
                    "lifecycle": "start after the module loads (unchanged declarations keep running across reloads), stop on unload; apt packages reinstall from the volume cache after each VM boot",
                    "views": "html view: <iframe src=\"oasvc://<module>/<port>/\"> (HTTP); WebSockets: host action service.url → ws URL; terminal: pty.open; Linux GUI: \"display\" + display.url",
                    "lan": "expose \"lan\": the user allows it once (consent card); then <Mac>:<port> on the local network (Bonjour name = module name)",
                })
            }),
        )],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_services() {
        let raw = json!({
            "services": [
                { "name": "web", "command": ["python3", "-m", "http.server", "8080"], "restart": "always",
                  "health": { "http": "http://127.0.0.1:8080/" }, "ports": [{ "port": 8080, "expose": "view" }, 9000] },
                { "name": "imap", "command": ["sh", "-c", "x"], "health": { "tcp": 1143 }, "env": { "A": "b" } }
            ],
            "apt": ["jq", "libfoo1:amd64=1.2-3", "vendor/bridge.deb"],
            "arch": "amd64"
        });
        let x = parse(&raw).unwrap();
        assert_eq!(x.services.len(), 2);
        assert_eq!(x.services[0].restart, Restart::Always);
        assert_eq!(x.services[0].ports, vec![PortSpec { port: 8080, expose: Expose::View }, PortSpec { port: 9000, expose: Expose::View }]);
        assert_eq!(x.services[1].restart, Restart::OnFailure);
        assert_eq!(x.services[1].health, Some(Health::Tcp(1143)));
        assert_eq!(x.apt.len(), 3);
        assert!(x.amd64);
        let bad = json!({ "services": [{ "name": "Web", "command": "python3" }, { "name": "a", "command": ["x"], "restart": "sometimes", "health": { "http": "https://example.com" } }], "apt": ["rm -rf /", "../x.deb"], "arch": "riscv" });
        let p = parse(&bad).unwrap_err();
        assert!(p.len() >= 6, "{p:?}");
        assert!(parse(&json!({ "name": "x" })).unwrap().is_empty());
    }
}
