//! Shared test support for `tests/kernel_host.rs`, `tests/vm_suite.rs` and `examples/gui_shell.rs`: the event sink,
//! `check!`, `wait_for`, scratch dirs, card auto-answers, the scratch VM home, and process isolation.
//!
//! Every suite case runs in its own child process of the test binary ([`isolated`]): the kernel reads its config
//! from process-wide env vars (state home, VM home, Keychain prefix, host-module build dir, idle / job intervals),
//! so each case gets its own scratch values without `unsafe` env changes and cases can run in parallel. The parent
//! removes the scratch dirs and any Keychain item the case left behind, also when it fails.
#![allow(dead_code)]

pub mod fixtures;

use openapp_lib::kernel::{EventSink, Kernel};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `PASS <msg>` or panic with `FAIL <msg>`. The condition is evaluated before the message.
macro_rules! check {
    ($cond:expr, $($msg:tt)*) => {{
        let ok: bool = $cond;
        let msg = format!($($msg)*);
        if ok { println!("PASS {msg}"); } else { panic!("FAIL {msg}"); }
    }};
}

// ---------------------------------------------------------------- event sink

pub type Forward = Box<dyn Fn(&str, &Value) + Send + Sync>;

/// Records every kernel event; `forward` (optional) also passes them on (e.g. to a Tauri webview).
#[derive(Default)]
pub struct Collect {
    events: Mutex<Vec<(String, Value)>>,
    pub forward: OnceLock<Forward>,
}

impl EventSink for Collect {
    fn emit(&self, event: &str, payload: Value) {
        if let Some(f) = self.forward.get() {
            f(event, &payload);
        }
        self.events.lock().unwrap().push((event.to_string(), payload));
    }
}

impl Collect {
    pub fn count(&self, ev: &str) -> usize {
        self.events.lock().unwrap().iter().filter(|(e, _)| e == ev).count()
    }
    pub fn last(&self, ev: &str) -> Option<Value> {
        self.events.lock().unwrap().iter().rev().find(|(e, _)| e == ev).map(|(_, p)| p.clone())
    }
    pub fn all(&self, ev: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|(e, _)| e == ev).map(|(_, p)| p.clone()).collect()
    }
    /// Chat items of a kind appended so far.
    pub fn appended(&self, kind: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, p)| e == "oa://chat" && p["op"] == "append" && p["item"]["kind"] == kind)
            .map(|(_, p)| p["item"].clone())
            .collect()
    }
    /// Permission cards appended so far.
    pub fn cards(&self) -> Vec<Value> {
        self.appended("permission")
    }
    /// Permission cards appended so far: (id, title).
    pub fn card_titles(&self) -> Vec<(String, String)> {
        self.cards().iter().map(|c| (c["id"].as_str().unwrap_or("").to_string(), c["title"].as_str().unwrap_or("").to_string())).collect()
    }
    /// Every payload as text (leak checks).
    pub fn all_text(&self) -> String {
        self.events.lock().unwrap().iter().map(|(_, p)| p.to_string()).collect::<Vec<_>>().join("\n")
    }
}

/// Answer kernel cards like a user would: `pick` returns the option id (None = cancel). Retries until the
/// card's waiter accepts the answer.
pub fn auto_answer(k: &Arc<Kernel>, sink: &Arc<Collect>, pick: impl Fn(&Value) -> Option<String> + Send + 'static) {
    let (k, sink) = (k.clone(), sink.clone());
    tokio::spawn(async move {
        let mut done = std::collections::HashSet::new();
        loop {
            for c in sink.cards() {
                let id = c["id"].as_str().unwrap_or("").to_string();
                if done.contains(&id) {
                    continue;
                }
                let opt = pick(&c);
                if k.respond_permission(&id, opt.clone()).is_ok() {
                    println!("  [card] {} -> {}", c["title"].as_str().unwrap_or(""), opt.as_deref().unwrap_or("cancel"));
                    done.insert(id);
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
}

/// The card's first option (allow / give / save).
pub fn first_option(c: &Value) -> Option<String> {
    Some(c["options"][0]["optionId"].as_str().unwrap_or("allow").to_string())
}

// ---------------------------------------------------------------- small helpers

/// Poll `f` every 50 ms for up to `secs`; the last result.
pub async fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

/// Poll `f` every 50 ms until it returns `Some` or `limit` passes.
pub async fn poll<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Newest open (unresolved) chat item of a kind.
pub async fn open_item(k: &Kernel, kind: &str, limit: Duration) -> Option<Value> {
    poll(limit, || k.snapshot().chat.iter().rev().find(|i| i["kind"] == kind && i.get("resolved").is_none()).cloned()).await
}

/// Answer a card the test just saw: its chat item is shown a moment before its waiter is registered, so retry
/// (up to 2 s) while the kernel says there is "no open" request.
pub async fn answer(mut f: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    let mut r = f();
    for _ in 0..100 {
        match &r {
            Err(e) if e.starts_with("no open") => tokio::time::sleep(Duration::from_millis(20)).await,
            _ => break,
        }
        r = f();
    }
    r
}

/// Wait until the agent is ready; an error status or the deadline fails the case.
pub async fn wait_ready(k: &Kernel, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        let (st, err) = k.agent_status();
        if st == "ready" {
            return;
        }
        if st == "error" || Instant::now() > deadline {
            panic!("FAIL agent {st}: {}", err.unwrap_or_default());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

pub fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// `modules/<id>/openapp.json` from `manifest` plus extra files.
pub fn write_module(ws: &Path, id: &str, manifest: Value, files: &[(&str, &str)]) -> anyhow::Result<()> {
    let d = ws.join("modules").join(id);
    std::fs::create_dir_all(d.join("views"))?;
    std::fs::write(d.join("openapp.json"), serde_json::to_string_pretty(&manifest)?)?;
    for (p, c) in files {
        let f = d.join(p);
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(f, c)?;
    }
    Ok(())
}

pub fn status(k: &Kernel, id: &str) -> String {
    k.snapshot().modules.iter().find(|m| m.id == id).map(|m| m.status.clone()).unwrap_or_default()
}

pub fn text_of(r: &rmcp::model::CallToolResult) -> String {
    openapp_lib::modules::result_text(r)
}

/// `<state>/research.jsonl` entries.
pub fn research(state: &Path) -> Vec<Value> {
    std::fs::read_to_string(state.join("research.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// All files below `dir`.
pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
        }
    }
    out
}

/// Files below `dir` whose bytes contain `needle`.
pub fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    walk(dir).into_iter().filter(|p| std::fs::read(p).is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle))).map(|p| p.display().to_string()).collect()
}

/// `@modelcontextprotocol/sdk` + `zod` for module servers in `ws` (installed once into a cache next to the test
/// target, linked as `ws/node_modules`).
pub fn mcp_sdk(ws: &Path) -> anyhow::Result<()> {
    let cache = option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("openapp-mcp-sdk");
    let pkg = r#"{"name":"smoke-ws","private":true,"type":"module"}"#;
    if !cache.join("node_modules/@modelcontextprotocol/sdk").exists() {
        println!("installing @modelcontextprotocol/sdk + zod into {}", cache.display());
        write(&cache.join("package.json"), pkg);
        let st = Command::new("npm").args(["i", "--silent", "--no-audit", "--no-fund", "@modelcontextprotocol/sdk", "zod"]).current_dir(&cache).status()?;
        anyhow::ensure!(st.success(), "npm install failed");
    }
    write(&ws.join("package.json"), pkg);
    let link = ws.join("node_modules");
    if !link.exists() {
        std::os::unix::fs::symlink(cache.join("node_modules"), &link)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- VM

/// Boot (or reuse) the app VM of `k` and print its timings.
pub async fn vm_up(k: &Arc<Kernel>) -> anyhow::Result<Arc<openapp_lib::vm::AppVm>> {
    let t = Instant::now();
    let vm = k.ensure_vm().await.map_err(anyhow::Error::msg)?;
    println!("VM {} up ({} ms): {}", vm.name, ms(t), vm.timings.lock().unwrap());
    Ok(vm)
}

/// The guest image for scratch VM homes: `OPENAPP_TEST_GUEST_TAR`, else `~/.openapp/vm/images/openapp-guest-<v>.tar`.
pub fn guest_tar() -> PathBuf {
    std::env::var_os("OPENAPP_TEST_GUEST_TAR").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
        dirs::home_dir().unwrap().join(".openapp/vm/images").join(format!("openapp-guest-{}.tar", openapp_lib::vm::GUEST_IMAGE_VERSION))
    })
}

/// A fresh, short (unix sockets) VM home with the guest image copied in (an APFS clone, no extra space).
fn scratch_vm_home(name: &str) -> PathBuf {
    let tag: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(10).collect();
    let home = PathBuf::from(format!("/tmp/oavm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let tar = guest_tar();
    assert!(tar.is_file(), "guest image {} not found: build it (node src-tauri/vm/guest-image/build.ts) or set OPENAPP_TEST_GUEST_TAR", tar.display());
    let dest = home.join("images").join(format!("openapp-guest-{}.tar", openapp_lib::vm::GUEST_IMAGE_VERSION));
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(&tar, &dest).unwrap();
    home
}

// ---------------------------------------------------------------- scratch + isolation

/// Scratch dirs and env of one case: workspace, state home, host-module builds, Keychain prefix, VM home.
/// Dropping it removes the dirs (kept with `OPENAPP_TEST_KEEP=1`) and every Keychain item recorded in the
/// scratch state's `secrets.json`.
pub struct Scratch {
    pub root: PathBuf,
    pub ws: PathBuf,
    pub state_home: PathBuf,
    pub vm_home: Option<PathBuf>,
    pub prefix: String,
}

impl Scratch {
    pub fn new(name: &str, vm: bool) -> Self {
        let root = std::env::temp_dir().join(format!("openapp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["ws", "state", "host-build"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        Scratch {
            ws: root.join("ws"),
            state_home: root.join("state"),
            vm_home: vm.then(|| scratch_vm_home(name)),
            prefix: format!("com.polygonface.openapp.test-{name}-{}", std::process::id()),
            root,
        }
    }

    /// The env a kernel of this case runs with.
    pub fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("OPENAPP_TEST_ROOT", self.root.clone().into()),
            ("OPENAPP_STATE_HOME", self.state_home.clone().into()),
            ("OPENAPP_HOST_BUILD", self.root.join("host-build").into()),
            ("OPENAPP_KEYCHAIN_PREFIX", self.prefix.clone().into()),
            // never the app's ~/.openapp/vm; without a VM home of its own a case has no VM
            ("OPENAPP_VM_HOME", self.vm_home.clone().unwrap_or_else(|| self.root.join("no-vm")).into()),
        ]
    }

    /// Delete Keychain items named in `<state>/<ws-id>/secrets.json` (service `<prefix>/<ws-id>/<name>`).
    fn keychain_cleanup(&self) -> usize {
        let mut removed = 0;
        for dir in std::fs::read_dir(&self.state_home).into_iter().flatten().flatten() {
            let ws_id = dir.file_name().to_string_lossy().to_string();
            let meta: Value = std::fs::read(dir.path().join("secrets.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
            for name in meta.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                let svc = format!("{}/{ws_id}/{name}", self.prefix);
                while Command::new("security").args(["delete-generic-password", "-s", &svc]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
                    removed += 1;
                }
            }
        }
        removed
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let n = self.keychain_cleanup();
        if n > 0 {
            println!("[cleanup] removed {n} Keychain item(s) left by the case");
        }
        if std::env::var("OPENAPP_TEST_KEEP").is_ok_and(|v| v == "1") {
            println!("[cleanup] kept {} (OPENAPP_TEST_KEEP=1)", self.root.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.root);
        if let Some(h) = &self.vm_home {
            let _ = std::fs::remove_dir_all(h);
        }
    }
}

/// What a case gets in its child process.
pub struct Ctx {
    pub root: PathBuf,
    pub ws: PathBuf,
    pub state_home: PathBuf,
}

#[derive(Default, Clone, Copy)]
pub struct Opts {
    /// Extra env for the case (e.g. short idle / job intervals).
    pub env: &'static [(&'static str, &'static str)],
    /// A scratch VM home with the guest image; VM cases run one at a time.
    pub vm: bool,
    /// Writes Keychain items: such cases run one at a time.
    pub keychain: bool,
}

const CHILD_ENV: &str = "OPENAPP_TEST_CHILD";
const DONE: &str = "OPENAPP-CASE-DONE";
static VM_LOCK: Mutex<()> = Mutex::new(());
static KEYCHAIN_LOCK: Mutex<()> = Mutex::new(());

/// Run `body` in a child process of this test binary with the case's scratch dirs and env. `name` must be the
/// `#[test]` function's name (the child re-runs exactly that test).
pub fn isolated<F, Fut>(name: &str, opts: Opts, body: F)
where
    F: FnOnce(Ctx) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
        let root = PathBuf::from(std::env::var_os("OPENAPP_TEST_ROOT").expect("OPENAPP_TEST_ROOT"));
        let ctx = Ctx { ws: root.join("ws"), state_home: root.join("state"), root };
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        if let Err(e) = rt.block_on(body(ctx)) {
            panic!("FAIL error: {e:#}");
        }
        rt.shutdown_timeout(Duration::from_secs(3));
        println!("{DONE} {name}");
        return;
    }
    let _vm = opts.vm.then(|| VM_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
    let _keychain = opts.keychain.then(|| KEYCHAIN_LOCK.lock().unwrap_or_else(|e| e.into_inner()));
    let t = Instant::now();
    let scratch = Scratch::new(name, opts.vm);
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([name, "--exact", "--quiet", "--nocapture", "--include-ignored", "--test-threads=1"])
        .env(CHILD_ENV, name)
        .env_remove("OPENAPP_HARNESS")
        .env_remove("OPENAPP_WORKSPACE")
        .env_remove("OPENAPP_GUEST_IMAGE_TAR");
    cmd.envs(scratch.env()).envs(opts.env.iter().copied());
    // the case's output goes through the harness: live with `-- --nocapture`, otherwise shown on failure
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn case");
    let mut stderr = child.stderr.take().unwrap();
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let (mut done, mut fail) = (false, None);
    for line in std::io::BufRead::lines(std::io::BufReader::new(child.stdout.take().unwrap())).map_while(Result::ok) {
        println!("{line}");
        done |= line == format!("{DONE} {name}");
        if fail.is_none() && line.contains("FAIL ") {
            fail = Some(line);
        }
    }
    let st = child.wait().expect("wait for case");
    let errs = errs.join().unwrap_or_default();
    eprint!("{errs}");
    let fail = fail.or_else(|| errs.lines().find(|l| l.contains("FAIL ")).map(String::from));
    drop(scratch);
    println!("TIMING case {name}: {} s", t.elapsed().as_secs());
    assert!(st.success() && done, "case {name} failed ({st}): {}", fail.unwrap_or_else(|| "see the output above".into()));
}
