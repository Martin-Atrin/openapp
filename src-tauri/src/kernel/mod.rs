//! The kernel core. No Tauri types here: events go out through `EventSink`.
//!
//! - `mod.rs`: types, construction, start / shutdown, the app VM, snapshot
//! - `chat.rs`: chat items, sessions, prompts, turns (§19.2), questions, permission answers, agent status
//! - `modules.rs`: module load / unload, tool calls, view refresh, the watcher tick, host-part consent (§18.1)
//! - `present.rs`: presentations (§19.1), web surfaces (§13), layout / theme / history
//! - `actions.rs`: host actions from views, the shell and the agent; UI errors
//! - `host.rs`: §18 host services (`host_call`, tokens, module-server env) and [`HostError`]
//! - `describe.rs`: `openapp_describe` and the `openapp_snapshot` text tree
//! - `grants.rs`: the grant store
//!
//! Built-in host actions live in `primitives/{chrome,audio,connect}.rs`; consent cards in `consent.rs`.

mod actions;
mod chat;
mod describe;
mod grants;
mod host;
mod modules;
mod present;

pub use chat::{DEFAULT_SESSION_TITLE, PermWaiter, QuestionWaiter, SessionState, TurnGuard, TurnRec, title_from_prompt};
pub use describe::KERNEL_GAPS;
pub use grants::GrantStore;
pub use host::{HostCode, HostError};
use chat::new_session_rec;
use present::view_index;

use crate::research::{Kind, Research};
use crate::util::MutexExt;
use crate::layout::{self, Layout, LayoutCtx};
use crate::modules::{Manifest, ModClient, Tail, ViewEntry};
use crate::state::{AgentInfo, HistEntry, ModuleInfo, ModulesMeta, SessionIndex, SessionMeta, SessionRec, Snapshot, Store, now_ts};
use crate::{theme, views, workspace};
use rmcp::model::{CallToolResult, Tool};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;
use tokio::sync::oneshot;

pub const CONTRACT_VERSION: &str = "0.10";

/// Kernel → shell events (`oa://snapshot`, `oa://chat`, `oa://composer`, `oa://notify`).
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: Value);
}

/// Rect in CSS px relative to the main webview (= logical px of the window content).
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct WebRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Native web surfaces (§13). Implemented by the Tauri layer with child webviews.
pub trait WebHost: Send + Sync + 'static {
    /// Create (first time), move and show (`Some`) or hide (`None`) the surface of a placement.
    fn set_bounds(&self, placement: &str, url: &str, profile: &str, rect: Option<WebRect>) -> Result<(), String>;
    fn navigate(&self, placement: &str, url: &str) -> Result<(), String>;
    /// -1 back, 1 forward, 0 reload
    fn history(&self, placement: &str, step: i32) -> Result<(), String>;
    fn destroy(&self, placement: &str);
    /// §20.3: evaluate a kernel script in the page; `done` gets the JSON-serialized result ("" on error).
    fn eval(&self, placement: &str, js: &str, done: Box<dyn FnOnce(String) + Send>) -> Result<(), String>;
}

/// Sink that drops events (tests, headless runs).
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: &str, _payload: Value) {}
}

pub struct ModuleEntry {
    pub id: String,
    pub name: String,
    pub status: String,
    pub error: Option<String>,
    pub manifest: Option<Manifest>,
    pub tools: Vec<Tool>,
    pub client: Option<Arc<ModClient>>,
    pub tail: Tail,
    pub views: BTreeMap<String, ViewEntry>,
    pub files: BTreeMap<PathBuf, SystemTime>,
    /// The server runs inside the app VM (§17).
    pub in_vm: bool,
    /// §18.1 host part (Swift sidecar on the Mac), when approved and built.
    pub host: Option<Arc<crate::host_services::Sidecar>>,
    /// Methods the host part reported (`methods`), for describe.
    pub host_methods: Value,
}

impl ModuleEntry {
    fn new(id: &str, name: String) -> Self {
        Self {
            id: id.into(),
            name,
            status: "stopped".into(),
            error: None,
            manifest: None,
            tools: Vec::new(),
            client: None,
            tail: Arc::new(Mutex::new(VecDeque::new())),
            views: BTreeMap::new(),
            files: BTreeMap::new(),
            in_vm: false,
            host: None,
            host_methods: Value::Null,
        }
    }
    fn tool_names(&self) -> BTreeSet<String> {
        self.tools.iter().map(|t| t.name.to_string()).collect()
    }
}

pub struct Inner {
    pub sessions: BTreeMap<String, SessionState>,
    pub active: String,
    pub layout: Layout,
    pub theme: Map<String, Value>,
    pub history: Vec<HistEntry>,
    pub meta: ModulesMeta,
    pub modules: BTreeMap<String, ModuleEntry>,
    pub ui_errors: VecDeque<Value>,
    /// placement id -> {url, title, loading} for live web surfaces
    pub web: BTreeMap<String, Value>,
    pub agent: AgentInfo,
    /// §19.1 presentations (kernel chrome): sheet / modal / inline views, top = last.
    pub presented: Vec<Value>,
    /// §20.3 guide strip `{placement?, steps: [{title, detail?}], current, done?}`.
    pub guide: Option<Value>,
}

pub struct McpEndpoint {
    pub url: String,
    pub token: String,
    pub port: u16,
}

/// Opens a URL in the system browser (set by the Tauri layer).
pub type UrlOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub struct Kernel {
    pub workspace: PathBuf,
    /// Kernel state outside the workspace (layout, theme, history, research log, sessions, harness, grants).
    pub state_dir: PathBuf,
    pub sink: Arc<dyn EventSink>,
    pub research: Research,
    pub store: Store,
    /// PATH for child processes (login-shell PATH merged; resolved once in `start`).
    path_cell: OnceLock<String>,
    /// ACP command of the `custom` harness (`OPENAPP_AGENT_CMD`).
    pub custom_cmd: Option<String>,
    pub inner: Mutex<Inner>,
    pub load_lock: tokio::sync::Mutex<()>,
    /// One turn at a time per session (turns in different sessions may overlap).
    pub turn_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Number of turns in flight (agent status is "working" while > 0).
    pub working: std::sync::atomic::AtomicUsize,
    pub mcp: OnceLock<McpEndpoint>,
    pub acp: Mutex<Option<Arc<crate::acp::AcpConn>>>,
    pub codex: Mutex<Option<Arc<crate::codex::CodexConn>>>,
    pub pi: Mutex<Option<Arc<crate::pi::PiConn>>>,
    /// Selected harness (None = provider picker).
    pub harness: Mutex<Option<crate::harness::HarnessSpec>>,
    /// Detected harnesses (§11).
    pub harnesses: Mutex<Vec<crate::harness::HarnessInfo>>,
    pub perms: Mutex<HashMap<String, PermWaiter>>,
    /// Native web surfaces (set by the Tauri layer; absent in headless runs).
    pub web_host: OnceLock<Arc<dyn WebHost>>,
    /// Placements that currently have a web surface.
    pub web_live: Mutex<BTreeSet<String>>,
    /// Open question cards: chat item id -> waiter (§12.4).
    pub questions: Mutex<HashMap<String, QuestionWaiter>>,
    /// The app VM (§17), while it runs.
    pub vm: Mutex<Option<Arc<crate::vm::AppVm>>>,
    vm_boot: tokio::sync::Mutex<()>,
    /// Last VM boot error (for describe / module errors).
    pub vm_error: Mutex<Option<String>>,
    /// Live harness state laid over detection (sign-in of VM harnesses).
    pub harness_overlay: Mutex<HashMap<String, crate::harness::Overlay>>,
    rt: OnceLock<tokio::runtime::Handle>,
    /// §18 `openapp-macos` sidecar (standard library), when the binary exists.
    pub host_std: Option<Arc<crate::host_services::Sidecar>>,
    /// Last `status` of the standard library (availability per service).
    pub host_status: Mutex<Value>,
    /// Bearer tokens for `POST /host/...`: token → caller ("module:<id>").
    host_tokens: Mutex<HashMap<String, String>>,
    /// §19.2 turns: session → queued (turnId, mode) not yet started; session → running turn.
    turn_queue: Mutex<HashMap<String, VecDeque<(String, String)>>>,
    turn_now: Mutex<HashMap<String, (String, String)>>,
    /// turnId → what a build turn changed (for turn.undo).
    pub turns: Mutex<BTreeMap<String, TurnRec>>,
    /// §20.1 secrets (Keychain + metadata).
    pub secrets: Arc<crate::secrets::SecretStore>,
    /// The user's remembered decisions (`<state>/grants.json`).
    pub grants: GrantStore,
    /// Opens URLs in the system browser (tests replace it).
    pub url_opener: OnceLock<UrlOpener>,
    /// Per-kernel state of primitives and kernel services (type map: egress decisions, secret cards, downloads, …).
    pub prims: crate::primitives::PrimState,
    me: std::sync::Weak<Kernel>,
    memory_bytes: Option<u64>,
    seq: AtomicU64,
    shutdown: AtomicBool,
}


fn memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let t = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = t.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Standard base64; padding optional (data URLs, clipboard images, bookmarks, Proxy-Authorization).
const B64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    B64.decode(s.trim()).ok()
}

pub fn open_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) {
        return Err(format!("open_url only opens http(s) and mailto links, got \"{url}\""));
    }
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    r.map(|_| ()).map_err(|e| format!("could not open URL: {e}"))
}

impl Kernel {
    /// Synchronous setup: init the workspace, load persisted state, discover modules.
    /// Harness: `OPENAPP_HARNESS` override, else the saved choice, else none (provider picker).
    pub fn new(workspace: PathBuf, sink: Arc<dyn EventSink>) -> anyhow::Result<Arc<Self>> {
        Self::new_opts(workspace, crate::harness::custom_from_env(), None, sink)
    }

    /// `custom_cmd`: ACP command for the `custom` harness. `force`: harness id to select now
    /// (not persisted; for tests).
    pub fn new_opts(
        workspace: PathBuf,
        custom_cmd: Option<String>,
        force: Option<&str>,
        sink: Arc<dyn EventSink>,
    ) -> anyhow::Result<Arc<Self>> {
        workspace::init(&workspace)?;
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        // kernel-owned state lives outside the VM-visible workspace
        let state_dir = crate::state::state_dir(&workspace);
        crate::state::ensure_dir(&workspace, &state_dir)?;
        let store = Store::new(&state_dir);
        let mut mods = BTreeMap::new();
        for id in workspace::module_dirs(&workspace) {
            let name = crate::modules::peek_name(&workspace.join("modules").join(&id), &id);
            mods.insert(id.clone(), ModuleEntry::new(&id, name));
        }
        // sessions: index + active id; first run creates one session
        let idx = store.load_session_index();
        let mut sessions = BTreeMap::new();
        for rec in idx.sessions {
            if rec.id.starts_with("s_") && layout::valid_ident(&rec.id) {
                sessions.insert(rec.id.clone(), SessionState { rec, chat: None, dirty: false });
            }
        }
        let mut created_first = false;
        if sessions.is_empty() {
            let rec = new_session_rec();
            sessions.insert(rec.id.clone(), SessionState { rec, chat: Some(Vec::new()), dirty: false });
            created_first = true;
        }
        let active = idx
            .active
            .filter(|a| sessions.contains_key(a))
            .unwrap_or_else(|| {
                let mut v: Vec<&SessionState> = sessions.values().collect();
                v.sort_by(|a, b| b.rec.updated.cmp(&a.rec.updated));
                v[0].rec.id.clone()
            });
        if let Some(s) = sessions.get_mut(&active)
            && s.chat.is_none() {
                s.chat = Some(store.load_session_chat(&active));
            }
        let inner = Inner {
            sessions,
            active,
            layout: store.load_layout(),
            theme: store.load_theme(),
            history: store.load_history(),
            meta: store.load_meta(),
            modules: mods,
            ui_errors: VecDeque::new(),
            web: BTreeMap::new(),
            presented: Vec::new(),
            guide: None,
            agent: AgentInfo {
                name: "No provider".into(),
                harness: None,
                status: "starting".into(), error: None, model: None, effort: None, model_name: None, tier: None },
        };
        if created_first {
            store.save_session_index(&inner.index());
        }
        let k = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            prims: Default::default(),
            research: Research::new(state_dir.join("research.jsonl")),
            state_dir: state_dir.clone(),
            store,
            path_cell: OnceLock::new(),
            custom_cmd,
            inner: Mutex::new(inner),
            load_lock: tokio::sync::Mutex::new(()),
            turn_locks: Mutex::new(HashMap::new()),
            working: std::sync::atomic::AtomicUsize::new(0),
            mcp: OnceLock::new(),
            acp: Mutex::new(None),
            codex: Mutex::new(None),
            pi: Mutex::new(None),
            harness: Mutex::new(None),
            harnesses: Mutex::new(Vec::new()),
            perms: Mutex::new(HashMap::new()),
            web_host: OnceLock::new(),
            web_live: Mutex::new(BTreeSet::new()),
            questions: Mutex::new(HashMap::new()),
            vm: Mutex::new(None),
            vm_boot: tokio::sync::Mutex::new(()),
            vm_error: Mutex::new(None),
            harness_overlay: Mutex::new(HashMap::new()),
            rt: OnceLock::new(),
            host_std: crate::host_services::std_binary().map(|b| {
                crate::host_services::Sidecar::new("openapp-macos", b, workspace.join(".openapp").join("logs").join("_host.log"), Vec::new(), None)
            }),
            host_status: Mutex::new(json!({ "available": false, "reason": "not checked yet" })),
            host_tokens: Mutex::new(HashMap::new()),
            turn_queue: Mutex::new(HashMap::new()),
            turn_now: Mutex::new(HashMap::new()),
            turns: Mutex::new(BTreeMap::new()),
            secrets: Arc::new(crate::secrets::SecretStore::new(&state_dir)),
            grants: GrantStore::new(&state_dir),
            url_opener: OnceLock::new(),
            memory_bytes: memory_bytes(),
            seq: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            workspace,
            sink,
        });
        let choice = match force {
            Some(id) => crate::harness::spec_by_id(k.custom_cmd.as_deref(), id),
            None => crate::harness::env_override(k.custom_cmd.as_deref()).or_else(|| crate::harness::load_saved(&k)),
        };
        if let Some(spec) = choice {
            let mut g = k.lock();
            g.agent.name = spec.name.clone();
            g.agent.harness = Some(spec.id.clone());
            drop(g);
            *k.harness.locked() = Some(spec);
        } else {
            k.lock().agent.status = "unselected".into();
        }
        Ok(k)
    }

    /// Async start: MCP server, watcher, module autoload, then the agent harness.
    pub async fn start(self: &Arc<Self>, start_agent: bool) -> anyhow::Result<()> {
        let _ = self.rt.set(tokio::runtime::Handle::current());
        let path = tokio::task::spawn_blocking(workspace::login_path).await.unwrap_or_default();
        let _ = self.path_cell.set(path.clone());
        let custom = self.custom_cmd.clone();
        let detection = tokio::spawn(async move { crate::harness::detect(&path, custom.as_deref()).await });
        let ep = crate::mcp_server::serve(self.clone()).await?;
        log::info!("openapp MCP server on {}", ep.url);
        let _ = self.mcp.set(ep);
        crate::primitives::event(self, &crate::primitives::Event::Started);
        tokio::spawn(workspace::watch(self.clone()));
        // §17: modules of a VM harness start inside the VM, so boot it before the autoload
        if start_agent
            && self.vm_required()
            && let Err(e) = self.ensure_vm().await
        {
            log::error!("app VM: {e}");
        }
        let ids: Vec<String> = self.lock().modules.keys().cloned().collect();
        let k = self.clone();
        if let Some(sc) = self.host_std.clone() {
            let k = self.clone();
            tokio::spawn(async move {
                let st = match sc.call("status", json!({}), std::time::Duration::from_secs(30)).await {
                    Ok(v) => json!({ "available": true, "services": v }),
                    Err(e) => json!({ "available": false, "reason": e }),
                };
                *k.host_status.locked() = st;
            });
        } else {
            *self.host_status.locked() =
                json!({ "available": false, "reason": "openapp-macos was not built (macOS + Swift 6 needed; set OPENAPP_MACOS_SERVICES)" });
        }
        let autoload = tokio::spawn(async move {
            for id in ids {
                crate::primitives::lifecycle::autoload(&k, &id).await;
            }
        });
        let has_choice = crate::harness::selected(self).is_some();
        if let Ok(list) = detection.await {
            self.set_harnesses(list);
        }
        if !has_choice {
            self.set_agent("unselected", None);
        } else if start_agent {
            tokio::spawn(crate::harness::run(self.clone()));
        } else {
            self.set_agent("error", Some("agent harness not started (headless run)".into()));
        }
        let _ = autoload.await;
        Ok(())
    }

    /// This kernel as an Arc (for primitives that spawn tasks from `&Kernel` hooks).
    pub fn arc(&self) -> Option<Arc<Kernel>> {
        self.me.upgrade()
    }

    pub fn path_env(&self) -> &str {
        self.path_cell.get().map(String::as_str).unwrap_or("/usr/local/bin:/usr/bin:/bin")
    }

    /// The running app VM.
    pub fn vm(&self) -> Option<Arc<crate::vm::AppVm>> {
        self.vm.locked().clone().filter(|v| v.is_alive())
    }

    /// The selected harness runs in the VM (then module servers must run there too).
    pub fn vm_required(&self) -> bool {
        crate::harness::selected(self).is_some_and(|s| s.vm)
    }

    /// Boot the app VM if it is not running; module servers running on the host move into it.
    pub async fn ensure_vm(self: &Arc<Self>) -> Result<Arc<crate::vm::AppVm>, String> {
        let _g = self.vm_boot.lock().await;
        if let Some(v) = self.vm() {
            return Ok(v);
        }
        let port = self.mcp.get().ok_or("kernel MCP server is not running")?.port;
        let weak = Arc::downgrade(self);
        let proxy: crate::vm::StreamHandler = Arc::new(move |s| {
            if let Some(k) = weak.upgrade() {
                tokio::spawn(crate::egress::serve(k, s));
            }
        });
        let weak = Arc::downgrade(self);
        let socks: crate::vm::StreamHandler = Arc::new(move |s| {
            if let Some(k) = weak.upgrade() {
                tokio::spawn(crate::egress::serve_socks(k, s));
            }
        });
        match crate::vm::AppVm::start(&self.workspace, port, proxy, socks).await {
            Ok(vm) => {
                *self.vm.locked() = Some(vm.clone());
                *self.vm_error.locked() = None;
                let timings = vm.timings.locked().clone();
                self.research.log("kernel", Kind::VmStart, json!({ "name": vm.name, "timings": timings, "network": "none (egress proxy only)" }));
                let on_host: Vec<String> = self
                    .lock()
                    .modules
                    .values()
                    .filter(|m| m.client.is_some() && !m.in_vm)
                    .map(|m| m.id.clone())
                    .collect();
                if !on_host.is_empty() {
                    let k = self.clone();
                    tokio::spawn(async move {
                        for id in on_host {
                            let _ = k.module_load(&id).await;
                        }
                    });
                }
                Ok(vm)
            }
            Err(e) => {
                *self.vm_error.locked() = Some(e.clone());
                self.research.log("kernel", Kind::VmError, json!({ "error": e }));
                Err(e)
            }
        }
    }

    /// Spawn on the kernel's runtime (safe from non-runtime threads, e.g. web view callbacks on the main thread).
    pub fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, f: F) {
        match self.rt.get() {
            Some(h) => {
                h.spawn(f);
            }
            None => {
                tokio::spawn(f);
            }
        }
    }

    pub fn secrets_handle(&self) -> Arc<crate::secrets::SecretStore> {
        self.secrets.clone()
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.locked()
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Snapshot {
        // §21: registry host data first (sources may read kernel state; never under the inner lock)
        let host_data = crate::primitives::host_data(self);
        let safe = crate::primitives::lifecycle::safe_mode(self);
        let models = crate::primitives::models::snapshot_models(self);
        let g = self.lock();
        // §23 (the models store has its own lock; it never takes the inner lock)
        let mode_now = g.sessions.get(&g.active).map(|s| s.rec.mode.clone()).unwrap_or_else(|| "chat".into());
        let model_prefs = crate::primitives::models::snapshot_prefs(self, g.agent.harness.as_deref(), &mode_now, &g.active);
        let mut agent = g.agent.clone();
        if let Some((m, e, name, tier)) = crate::primitives::models::agent_fields(self, g.agent.harness.as_deref(), &mode_now, &g.active) {
            agent.model = Some(m);
            agent.effort = e;
            agent.model_name = Some(name);
            agent.tier = Some(tier.into());
        }
        let mut views = BTreeMap::new();
        for (id, m) in &g.modules {
            for (vn, v) in &m.views {
                views.insert(format!("{id}/{vn}"), v.value.clone());
            }
        }
        let mut placements = g.layout.placements.clone();
        placements.sort_by(|a, b| a.order.partial_cmp(&b.order).unwrap_or(std::cmp::Ordering::Equal));
        Snapshot {
            // §21 G9 safe mode: layout hidden, kernel chrome only (the saved layout is kept)
            layout: if safe { Layout::default() } else { Layout { regions: g.layout.regions.clone(), placements } },
            host_data,
            theme: theme::resolve(&g.theme),
            modules: g
                .modules
                .values()
                .map(|m| ModuleInfo { id: m.id.clone(), name: m.name.clone(), status: m.status.clone(), error: m.error.clone() })
                .collect(),
            views,
            agent,
            models,
            model_prefs,
            chat: g.active_chat().to_vec(),
            sessions: g.session_metas(),
            harnesses: self.harnesses.locked().clone(),
            active_session: g.active.clone(),
            active_mode: g.sessions.get(&g.active).map(|s| s.rec.mode.clone()).unwrap_or_else(|| "chat".into()),
            web: g.web.clone(),
            grants: self.grants_list(),
            presented: if safe { Vec::new() } else { g.presented.clone() },
            secrets: self.secrets.list(),
            guide: g.guide.clone(),
            turns: {
                let t = self.turns.locked();
                t.iter()
                    .filter(|(_, r)| r.session == g.active && r.mode == "build")
                    .map(|(id, r)| json!({ "id": id, "changes": r.changes, "modules": r.modules, "undone": r.undone, "undoable": !r.undone && (r.changes > 0 || !r.modules.is_empty()) }))
                    .collect()
            },
        }
    }

    pub fn emit_snapshot(&self) {
        self.sync_web();
        let snap = self.snapshot();
        crate::primitives::event(self, &crate::primitives::Event::Snapshot(&snap));
        self.sink.emit("oa://snapshot", serde_json::to_value(snap).unwrap_or(Value::Null));
    }

    pub fn notify(&self, text: &str) {
        self.sink.emit("oa://notify", json!({ "text": text }));
        crate::primitives::event(self, &crate::primitives::Event::Notify(text));
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        crate::primitives::event(self, &crate::primitives::Event::Shutdown);
        self.flush_sessions();
        let clients: Vec<_> = self.lock().modules.values_mut().filter_map(|m| m.client.take()).collect();
        for c in clients {
            c.cancellation_token().cancel();
        }
        crate::harness::kill(self);
        // §18 sidecars
        if let Some(h) = &self.host_std {
            h.kill();
        }
        let hosts: Vec<_> = self.lock().modules.values_mut().filter_map(|m| m.host.take()).collect();
        for h in hosts {
            h.kill();
        }
        // §8: the VM stops on quit (bounded wait; the runtime also exits with its parent). The host sockets go
        // first, synchronously, so an interrupted stop never leaves the VM home looking in use.
        let vm = self.vm.locked().take();
        if let Some(vm) = &vm {
            vm.close_sockets();
        }
        if let (Some(vm), Some(rt)) = (vm, self.rt.get().cloned()) {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                rt.block_on(async move { vm.stop().await });
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
        }
    }
}
