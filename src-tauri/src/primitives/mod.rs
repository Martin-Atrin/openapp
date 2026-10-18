//! Primitive registry (contract §21). Each primitive lives in `primitives/<name>.rs` and returns one
//! [`Primitive`] from `primitive()`: its host actions, MCP tools, `POST /host/<method>` endpoints, host data
//! sources, Tauri commands (`primitive_command`), slash commands, module-server env, extra HTTP routes,
//! kernel event hooks and its describe entry. The kernel consults the registry after its built-ins, so a new
//! primitive needs one line in [`build`] and no kernel edit. See README.md in this folder.

use crate::util::MutexExt;
use crate::kernel::{HostError, Kernel};
use serde_json::{Map, Value, json};
use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

pub mod channels;
pub mod lifecycle;
pub mod services; // G2 services + apt + amd64 (builder B)
pub mod ports; // G2 oasvc:// + gateway + LAN (builder B)
pub mod pty; // G2 PTY (builder B)
pub mod display; // G3 virtual display (builder B)
pub mod shell; // G4 native shell: shared state (builder C)
pub mod shell_windows; // G4 windows
pub mod shell_menu; // G4 menu bar
pub mod shell_tray; // G4 tray item
pub mod shell_shortcuts; // G4 shortcuts
pub mod shell_notify; // G4 notifications with actions
pub mod shell_misc; // G4 dock badge, clipboard, print, share, drag and drop, deep links, file handlers
pub mod media; // G4 microphone + camera in module html views (consent + WKWebView capture gate)
pub mod files; // G5 files and folder grants
pub mod jobs; // G6 jobs registry (builder D)
pub mod background; // G6 background + login item
pub mod secret_release; // G7 secret release + generate
pub mod host_service; // G8 long-running host modules + AppKit runtime
pub mod agents; // G10 agent modules
pub mod audio; // §15.1 microphone recordings (built-in action)
pub mod chrome; // kernel chrome (built-in actions)
pub mod connect; // §20 secrets, net, OAuth, web control, guide (built-in actions + kernel methods)
pub mod models; // §23 discovered models, choice, escalation

/// The registration list: one line per primitive.
fn build() -> Vec<Primitive> {
    vec![
        channels::primitive(),
        lifecycle::primitive(),
        services::primitive(),
        pty::primitive(),
        display::primitive(),
        shell_windows::primitive(),
        shell_menu::primitive(),
        shell_tray::primitive(),
        shell_shortcuts::primitive(),
        shell_notify::primitive(),
        shell_misc::primitive(),
        media::primitive(),
        files::primitive(),
        jobs::primitive(),
        background::primitive(),
        secret_release::primitive(),
        host_service::primitive(),
        agents::primitive(),
        models::primitive(),
    ]
}

/// A built-in host action of the kernel (chrome, audio, §20 connect). Unlike registry actions it runs synchronously
/// from [`Kernel::host_action`]: long work is spawned and its errors become toasts.
pub struct Builtin {
    pub name: &'static str,
    /// one-line args doc for describe (`""` = not listed in describe.host_actions)
    pub args: &'static str,
    /// string keys a spec view action must carry (checked when the view is read)
    pub keys: &'static [&'static str],
    pub run: fn(&Arc<Kernel>, &[&Value]) -> Result<(), String>,
}

/// The kernel's built-in host actions, in contract order (chrome, audio, connect).
pub fn builtins() -> impl Iterator<Item = &'static Builtin> {
    chrome::ACTIONS.iter().chain(audio::ACTIONS).chain(connect::ACTIONS)
}

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    builtins().find(|b| b.name == name)
}

/// Names of the built-in host actions (`capabilities.host_actions`, view checks, error hints).
pub fn host_actions() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| builtins().map(|b| b.name).collect())
}

/// `openapp_describe → host_actions` for the built-ins (registry actions are added by [`extend_describe`]).
pub fn builtin_docs() -> Value {
    Value::Object(builtins().filter(|b| !b.args.is_empty()).map(|b| (b.name.to_string(), json!(b.args))).collect())
}

/// First string value of `key` in the action object / input (later sources are fallbacks).
pub fn src_str(src: &[&Value], key: &str) -> Option<String> {
    src.iter().find_map(|v| v.get(key).and_then(Value::as_str).map(String::from))
}

/// All keys of the action object and input merged (later wins), without `host`.
pub fn src_params(src: &[&Value]) -> Map<String, Value> {
    let mut params = Map::new();
    for v in src {
        if let Some(o) = v.as_object() {
            params.extend(o.iter().filter(|(key, _)| key.as_str() != "host").map(|(a, b)| (a.clone(), b.clone())));
        }
    }
    params
}

/// Consent class (§21): Free (no card), Granted (one-time grant), Confirmed (card every time).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum Class {
    #[default]
    Free,
    Granted,
    Confirmed,
}

/// Who calls: `agent`, `user` (kernel chrome), `kernel`, `module:<id>` (module server), `view:<id>`
/// (a view of module id), `host:<id>` (host part of module id). `placement` is set for view calls.
#[derive(Clone, Debug)]
pub struct Caller {
    pub id: String,
    pub placement: Option<String>,
}

impl Caller {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), placement: None }
    }
    /// The module behind the caller (module:/view:/host:), if any.
    pub fn module(&self) -> Option<&str> {
        ["module:", "view:", "host:"].iter().find_map(|p| self.id.strip_prefix(p))
    }
    /// The caller for a placement (`view:<module>`, or `user` for kernel chrome).
    pub fn for_placement(k: &Kernel, placement: &str) -> Self {
        let id = match k.placement_view(placement) {
            Some((m, _)) => format!("view:{m}"),
            None => "user".into(),
        };
        Self { id, placement: Some(placement.to_string()) }
    }
}

pub type Fut = Pin<Box<dyn Future<Output = Result<Value, HostError>> + Send>>;
/// Async handler: (kernel, params object, caller) → result JSON. Errors are typed for `POST /host/<method>`
/// (plain strings are [`crate::kernel::HostCode::Upstream`]); actions and MCP tools see the message.
pub type Handler = Arc<dyn Fn(Arc<Kernel>, Value, Caller) -> Fut + Send + Sync>;
pub type DataFn = Arc<dyn Fn(&Kernel) -> Value + Send + Sync>;
pub type EnvFn = Arc<dyn Fn(&Kernel, &str, bool) -> Vec<(String, String)> + Send + Sync>;
pub type SlashFn = Arc<dyn Fn(&Arc<Kernel>, &str) + Send + Sync>;
pub type EventFn = Arc<dyn Fn(&Kernel, &Event) + Send + Sync>;

/// Wrap an async fn as a [`Handler`] (its error: `String` or [`HostError`]).
pub fn handler<F, R, E>(f: F) -> Handler
where
    F: Fn(Arc<Kernel>, Value, Caller) -> R + Send + Sync + 'static,
    R: Future<Output = Result<Value, E>> + Send + 'static,
    E: Into<HostError>,
{
    Arc::new(move |k, v, c| {
        let fut = f(k, v, c);
        Box::pin(async move { fut.await.map_err(Into::into) })
    })
}

pub fn data_fn(f: impl Fn(&Kernel) -> Value + Send + Sync + 'static) -> DataFn {
    Arc::new(f)
}

/// A host action (`{ "host": name, ... }` from views; also `openapp_host_action` for the agent).
pub struct Action {
    pub name: &'static str,
    /// one-line args doc for describe, e.g. `"{ topic, data }"`
    pub args: &'static str,
    pub handler: Handler,
}

pub struct McpTool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema of the arguments (an object schema)
    pub schema: Value,
    pub handler: Handler,
}

/// `POST /host/<method>` for module servers (and `host_call` without module).
pub struct Endpoint {
    pub method: &'static str,
    pub args: &'static str,
    pub handler: Handler,
}

/// `{ "$host": name }` in spec views (shipped in the snapshot as `hostData[name]`).
pub struct DataSource {
    pub name: &'static str,
    pub doc: &'static str,
    pub get: DataFn,
}

/// Kernel events delivered to every primitive's `on_event`.
pub enum Event<'a> {
    /// `Kernel::start` finished its setup (MCP server up; autoload may still run).
    Started,
    ModuleLoaded(&'a str),
    ModuleUnloaded(&'a str),
    /// A snapshot is about to be sent to the shell.
    Snapshot(&'a crate::state::Snapshot),
    Notify(&'a str),
    Shutdown,
}

#[derive(Default)]
pub struct Primitive {
    pub name: &'static str,
    pub purpose: &'static str,
    pub example: &'static str,
    pub class: Class,
    pub actions: Vec<Action>,
    pub tools: Vec<McpTool>,
    pub endpoints: Vec<Endpoint>,
    pub data: Vec<DataSource>,
    /// Tauri `primitive_command { name, args, placement_id? }`.
    pub commands: Vec<(&'static str, Handler)>,
    /// Chat commands: (`/name`, handler(kernel, rest of the line)).
    pub slash: Vec<(&'static str, SlashFn)>,
    /// Extra env for module servers: (kernel, module id, in_vm).
    pub env: Option<EnvFn>,
    /// Extra routes on the kernel HTTP server (reachable from the VM through the MCP forwarder).
    pub router: Option<fn(Arc<Kernel>) -> axum::Router>,
    pub on_event: Option<EventFn>,
    /// Extra top-level keys in `openapp_describe` (never overrides a kernel key).
    pub describe_keys: Vec<(&'static str, DataFn)>,
}

static REG: OnceLock<Vec<Primitive>> = OnceLock::new();

pub fn all() -> &'static [Primitive] {
    REG.get_or_init(build)
}

pub fn action(name: &str) -> Option<&'static Action> {
    all().iter().flat_map(|p| p.actions.iter()).find(|a| a.name == name)
}
pub fn tool(name: &str) -> Option<&'static McpTool> {
    all().iter().flat_map(|p| p.tools.iter()).find(|t| t.name == name)
}
pub fn endpoint(method: &str) -> Option<&'static Endpoint> {
    all().iter().flat_map(|p| p.endpoints.iter()).find(|e| e.method == method)
}
pub fn command(name: &str) -> Option<&'static Handler> {
    all().iter().flat_map(|p| p.commands.iter()).find(|(n, _)| *n == name).map(|(_, h)| h)
}
pub fn is_data_source(name: &str) -> bool {
    all().iter().flat_map(|p| p.data.iter()).any(|d| d.name == name)
}

/// Host data of all primitives (`snapshot.hostData`). Must not hold the kernel's inner lock.
pub fn host_data(k: &Kernel) -> BTreeMap<String, Value> {
    all().iter().flat_map(|p| p.data.iter()).map(|d| (d.name.to_string(), (d.get)(k))).collect()
}

pub fn mcp_tools() -> Vec<rmcp::model::Tool> {
    all()
        .iter()
        .flat_map(|p| p.tools.iter())
        .map(|t| {
            let schema = match &t.schema {
                Value::Object(m) => Arc::new(m.clone()),
                _ => Arc::new(Map::new()),
            };
            rmcp::model::Tool::new(t.name, t.description, schema)
        })
        .collect()
}

fn params(v: Value) -> Value {
    if v.is_null() { json!({}) } else { v }
}

pub async fn call_tool(k: &Arc<Kernel>, name: &str, args: Value) -> Option<Result<Value, String>> {
    let t = tool(name)?;
    Some((t.handler)(k.clone(), params(args), Caller::new("agent")).await.map_err(String::from))
}
pub async fn call_action(k: &Arc<Kernel>, name: &str, args: Value, caller: Caller) -> Option<Result<Value, String>> {
    let a = action(name)?;
    let mut args = params(args);
    if let Some(o) = args.as_object_mut() {
        o.remove("host");
    }
    Some((a.handler)(k.clone(), args, caller).await.map_err(String::from))
}
pub async fn call_endpoint(k: &Arc<Kernel>, method: &str, args: Value, caller: &str) -> Option<Result<Value, HostError>> {
    let e = endpoint(method)?;
    Some((e.handler)(k.clone(), params(args), Caller::new(caller)).await)
}

/// Run a chat command (`/safe`, …). Returns true when one matched.
pub fn slash(k: &Arc<Kernel>, text: &str) -> bool {
    for p in all() {
        for (cmd, f) in &p.slash {
            if let Some(rest) = text.strip_prefix(cmd)
                && (rest.is_empty() || rest.starts_with(' '))
            {
                f(k, rest.trim());
                return true;
            }
        }
    }
    false
}

pub fn event(k: &Kernel, ev: &Event) {
    for p in all() {
        if let Some(f) = &p.on_event {
            f(k, ev);
        }
    }
}

pub fn env(k: &Kernel, module: &str, in_vm: bool) -> Vec<(String, String)> {
    all().iter().filter_map(|p| p.env.as_ref()).flat_map(|f| f(k, module, in_vm)).collect()
}

pub fn router(k: Arc<Kernel>) -> axum::Router {
    all().iter().filter_map(|p| p.router).fold(axum::Router::new(), |r, f| r.merge(f(k.clone())))
}

/// `openapp_describe → primitives`.
pub fn describe() -> Value {
    Value::Array(
        all()
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "purpose": p.purpose,
                    "example": p.example,
                    "class": p.class,
                    "host_actions": p.actions.iter().map(|a| json!({ "name": a.name, "args": a.args })).collect::<Vec<_>>(),
                    "mcp_tools": p.tools.iter().map(|t| t.name).collect::<Vec<_>>(),
                    "host_endpoints": p.endpoints.iter().map(|e| json!({ "method": e.method, "args": e.args, "call": format!("POST $OPENAPP_HOST_URL/{}", e.method) })).collect::<Vec<_>>(),
                    "host_data": p.data.iter().map(|d| json!({ "name": d.name, "doc": d.doc })).collect::<Vec<_>>(),
                    "chat_commands": p.slash.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Merge the primitives' describe keys and host data docs into a describe object.
pub fn extend_describe(k: &Kernel, d: &mut Value) {
    if let Some(o) = d.as_object_mut() {
        o.insert("primitives".into(), describe());
        for p in all() {
            for (key, f) in &p.describe_keys {
                if !o.contains_key(*key) {
                    o.insert((*key).to_string(), f(k));
                }
            }
        }
    }
    if let Some(src) = d.pointer_mut("/host_data/sources").and_then(Value::as_object_mut) {
        for ds in all().iter().flat_map(|p| p.data.iter()) {
            src.entry(ds.name).or_insert_with(|| json!(ds.doc));
        }
    }
    if let Some(ha) = d.get_mut("host_actions").and_then(Value::as_object_mut) {
        for a in all().iter().flat_map(|p| p.actions.iter()) {
            ha.entry(a.name).or_insert_with(|| json!(a.args));
        }
    }
}

/// Per-kernel state of primitives (a type map): `k.prims.get::<MyState>()`.
#[derive(Default)]
pub struct PrimState(Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>);

impl PrimState {
    pub fn get<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        let mut m = self.0.locked();
        let v = m.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(T::default()) as Arc<dyn Any + Send + Sync>).clone();
        drop(m);
        v.downcast::<T>().unwrap_or_else(|_| unreachable!("type map holds T under TypeId::of::<T>"))
    }
}

/// String param helper for handlers.
pub fn arg<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
