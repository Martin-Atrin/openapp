//! Pi harness (contract §11, §17): `pi --mode rpc` (github.com/earendil-works/pi, MIT,
//! `@earendil-works/pi-coding-agent`) inside the app VM. Protocol reference: the package's
//! `docs/rpc.md`, `docs/rpc-commands.md`, `docs/json.md`, `docs/rpc-extension-ui.md`.
//!
//! - Install: pinned Pi with npm (through the egress proxy, actor `kernel`) into
//!   `/opt/openapp/runtimes/pi/<ver>`; agent dir (`PI_CODING_AGENT_DIR`) `/opt/openapp/home/pi`.
//! - The kernel owns the agent dir config: `SYSTEM.md` (OpenApp's framing replaces Pi's whole system
//!   prompt), `mcp.json` (kernel MCP server through the guest forwarder, bearer token from env),
//!   `settings.json` (merged: skills path, no project trust, no telemetry), `extensions/openapp.ts`
//!   (`ask_user` question tool) and `openapp-login.ts` (OAuth helper).
//! - One Pi process; Pi has one active session at a time, so turns are serialized and each OpenApp
//!   session maps to a Pi session file (stored per harness like Codex threads; `new_session` /
//!   `switch_session`). Turn end = `agent_settled`; cancel = `abort`.
//! - Extension UI requests become §12.4 question cards (`select`, `input`, `editor`; `ask_user`
//!   carries the full question list) or permission cards (`confirm`).
//! - Sign-in inside the VM: Pi's own OAuth (ChatGPT or Claude) with the callback port forwarded from
//!   the host (or the pasted redirect URL), or an API key written to the VM's `auth.json`.

use crate::research::Kind;
use crate::rpc::{self, Peer, Wait};
use crate::util::{MutexExt, short, tail_chars};
use crate::harness::{self, cancel_open_permissions, turn_finished, turn_started};
use crate::kernel::Kernel;
use crate::modules::{self, Tail};
use crate::vm::{GUEST_HOME, GUEST_OPT, GUEST_WS};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const PI_VERSION: &str = "1.3.0";
pub const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";
/// Pi agent dir in the guest (on the persistent VM volume).
pub const VM_PI_HOME: &str = "/opt/openapp/home/pi";
const EXTENSION_TS: &str = include_str!("../../workspace-template/pi/openapp.ts");
const LOGIN_TS: &str = include_str!("../../workspace-template/pi/login.ts");
/// Title prefix of `ask_user` requests (workspace-template/pi/openapp.ts).
const ASK_MARK: &str = "openapp.ask:";
const OUTPUT_TAIL: usize = 4000;

/// Sign-in choices on the picker card: method id, Pi provider, label, OAuth callback port.
const OAUTH: &[(&str, &str, &str, u16)] = &[("chatgpt", "openai", "ChatGPT", 1455), ("claude", "anthropic", "Claude", 53692)];
/// API-key providers offered on the card (their API hosts are on the egress allowlist).
const API_KEY_PROVIDERS: &[(&str, &str)] = &[("openai", "OpenAI"), ("anthropic", "Anthropic")];

/// §23: how often a running Pi discovers its account's models again.
const MODELS_REFRESH: Duration = Duration::from_secs(6 * 3600);

/// Prefix of the `/openapp-models` reply (`notify` message; workspace-template/pi/openapp.ts).
const MODELS_MARK: &str = "openapp.models:";
/// Test transport: run this host command as the Pi RPC process instead of Pi in the app VM.
pub const TEST_CMD_ENV: &str = "OPENAPP_PI_CMD";

pub fn runtime_dir() -> String {
    format!("{GUEST_OPT}/runtimes/pi/{PI_VERSION}")
}
pub fn bin() -> String {
    format!("{}/bin/pi", runtime_dir())
}
pub fn pkg_dir() -> String {
    format!("{}/lib/node_modules/{PI_PACKAGE}", runtime_dir())
}

/// Env of every Pi process in the guest: agent dir, no update checks / telemetry / catalog
/// refreshes (they would show egress cards for pi.dev), proxy, and the MCP token when given.
pub fn guest_env(token: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![
        ("PI_CODING_AGENT_DIR".to_string(), VM_PI_HOME.to_string()),
        ("HOME".to_string(), GUEST_HOME.to_string()),
        ("PI_OFFLINE".to_string(), "1".to_string()),
        ("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string()),
        ("PI_TELEMETRY".to_string(), "0".to_string()),
        // §23: the ChatGPT account list is versioned like Codex's own client
        ("OPENAPP_CODEX_CLIENT_VERSION".to_string(), crate::codex::CODEX_VERSION.to_string()),
    ];
    if let Some(t) = token {
        env.push((crate::codex::TOKEN_ENV.to_string(), t.to_string()));
    }
    env.extend(crate::vm::proxy_env("agent"));
    env
}

/// `OPENAPP_AGENT_CLI` for agent modules when Pi is the harness (same login, no MCP/extensions).
pub fn agent_cli() -> String {
    format!("{} -p --mode json --no-session --no-mcp --no-extensions", bin())
}

const TOOLS: &str = "Your tools: read, bash, edit, write, grep, find and ls act on the VM (cwd /workspace). ask_user is the question tool: one to four questions, each with options and/or a typed answer. The openapp MCP tools are named mcp__openapp__<tool>, for example mcp__openapp__openapp_describe (call it first) and mcp__openapp__openapp_secret_request. In build mode the openapp-builder skill is loaded into the message.";

/// Pi's whole system prompt (`<agent dir>/SYSTEM.md`): OpenApp's framing (§12.3, §17, §12.1, §19.2).
/// Pi still appends AGENTS.md, the skills list and the cwd.
pub fn system_prompt() -> String {
    format!(
        "{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}",
        harness::CONTAINMENT,
        harness::VM_RULE,
        harness::MODE_RULE,
        harness::BUILD_RULE,
        TOOLS,
        crate::mcp_server::INSTRUCTIONS
    )
}

fn mcp_json() -> Value {
    json!({
        "mcpServers": {
            "openapp": {
                "url": format!("http://127.0.0.1:{}/mcp", crate::vm::MCP_PORT),
                "headers": { "Authorization": format!("Bearer ${{{}}}", crate::codex::TOKEN_ENV) },
                "exposure": "direct",
                "timeout": 600,
                "description": "OpenApp kernel: build and change the user's app (modules, views, layout, theme), secrets, network and macOS host services."
            }
        }
    })
}

/// Settings the kernel owns (merged into `settings.json`, which Pi also writes, e.g. `deviceId`).
fn base_settings() -> Map<String, Value> {
    let v = json!({
        "defaultProjectTrust": "never",
        "enableInstallTelemetry": false,
        "quietStartup": true,
        "skills": [crate::vm::GUEST_WS.to_string() + "/.openapp/skills"],
        "defaultTools": ["+grep", "+find", "+ls"],
        "extensions": ["-builtin:llama.cpp"]
    });
    v.as_object().cloned().unwrap_or_default()
}

/// Build-mode message (§12.1): Pi's native skill expansion (`/skill:<name> <args>` inlines the
/// skill like Codex's skill input item; the build rules are in the system prompt) and the user's text.
fn turn_message(text: &str, build: bool) -> String {
    if !build {
        return text.to_string();
    }
    format!("/skill:{} [OpenApp build mode] {text}", crate::workspace::SKILL_NAME)
}

#[derive(Default)]
struct PTurn {
    oa: String,
    done: Option<oneshot::Sender<Value>>,
    /// all assistant text of the turn
    text: String,
    /// content block of the current assistant message ("text:0", "thinking:1") -> chat item id
    blocks: HashMap<String, String>,
    /// toolCallId -> (chat item id, last output emit)
    tools: HashMap<String, (String, Instant)>,
    /// last assistant error (cleared by a later successful message)
    error: Option<String>,
}

struct LoginFlow {
    provider: String,
    method: String,
    tx: mpsc::UnboundedSender<String>,
    /// open `manual_code` prompt (a pasted redirect URL answers it)
    prompt: Option<u64>,
    killer: crate::vm::GuestKill,
    _forward: Option<crate::vm::HostForward>,
}

/// Stops the Pi process: the guest process, or the host test process group.
#[derive(Clone)]
enum Killer {
    Guest(crate::vm::GuestKill),
    Host(Option<u32>),
}

impl Killer {
    fn kill(&self) {
        match self {
            Killer::Guest(g) => g.kill(),
            Killer::Host(Some(pid)) => harness::kill_group(*pid),
            Killer::Host(None) => {}
        }
    }
}

pub struct PiConn {
    peer: Peer,
    killer: Killer,
    /// Runs as a host test process (`OPENAPP_PI_CMD`), not in the app VM.
    host: bool,
    /// §23 waiter of an `/openapp-models` reply.
    models_reply: Mutex<Option<oneshot::Sender<Value>>>,
    /// §23 `provider/id:effort` last written as Pi's startup default (agent modules start with it).
    settings_model: Mutex<Option<String>>,
    pub harness_id: String,
    pub tail: Tail,
    /// Pi runs one session at a time: turns of different OpenApp sessions wait for each other.
    run_lock: tokio::sync::Mutex<()>,
    /// OpenApp session whose Pi session is loaded.
    current: Mutex<Option<String>>,
    turn: Mutex<Option<PTurn>>,
    login: Mutex<Option<LoginFlow>>,
    /// `notify` messages from Pi and extensions (latest 50).
    notices: Mutex<VecDeque<String>>,
}

impl PiConn {
    fn send(&self, v: Value) {
        self.peer.send(&v);
    }

    /// Send an RPC command; returns its `data` (or the `error` of a failed command).
    pub async fn request(&self, mut cmd: Value, timeout: Duration) -> Result<Value, String> {
        let id = format!("oa{}", self.peer.next_id());
        let ty = cmd.get("type").and_then(Value::as_str).unwrap_or("command").to_string();
        cmd["id"] = json!(id);
        match self.peer.call(id, &cmd, timeout).await {
            Ok(v) if v.get("success").and_then(Value::as_bool) == Some(true) => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
            Ok(v) => Err(v.get("error").and_then(Value::as_str).unwrap_or("command failed").to_string()),
            Err(Wait::Closed) => Err("Pi closed the connection".into()),
            Err(Wait::Timeout) => Err(format!("{ty} timed out after {}s", timeout.as_secs())),
        }
    }

    pub fn terminate(&self) {
        self.killer.kill();
        if let Some(f) = self.login.locked().take() {
            f.killer.kill();
        }
    }

    /// Latest `notify` messages (diagnostics, `/openapp-check` replies).
    pub fn notices(&self) -> Vec<String> {
        self.notices.locked().iter().cloned().collect()
    }

    fn turn_oa(&self) -> Option<String> {
        self.turn.locked().as_ref().map(|t| t.oa.clone())
    }
}

pub fn conn(k: &Kernel) -> Option<Arc<PiConn>> {
    k.pi.locked().clone()
}

// ---------- install + config ----------

/// Install the pinned Pi into the VM volume (once; cached on /opt/openapp). Returns the binary.
pub async fn install_in_vm(k: &Arc<Kernel>, vm: &crate::vm::AppVm, harness_id: &str) -> Result<String, String> {
    let (dir, bin) = (runtime_dir(), bin());
    let check = format!("test -x {bin} && test -f {}/dist/index.js", pkg_dir());
    let verify = format!("PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0 PI_CODING_AGENT_DIR={VM_PI_HOME} {bin} --version");
    let npm_spec = format!("{PI_PACKAGE}@{PI_VERSION}");
    harness::install_in_vm(k, vm, harness_id, &harness::NpmInstall { name: "Pi", version: PI_VERSION, npm_spec: &npm_spec, dir: &dir, check: &check, verify: &verify }).await?;
    Ok(bin)
}

/// Read a JSON file in the guest (None when missing or invalid). Never logged.
async fn read_guest_json(vm: &crate::vm::AppVm, path: &str) -> Option<Value> {
    let (code, out, _) = vm.sh(&format!("cat {path} 2>/dev/null"), &[]).await.ok()?;
    if code != 0 {
        return None;
    }
    serde_json::from_str(&out).ok()
}

/// Merge keys into `<agent dir>/settings.json` (Pi keeps its own keys there, e.g. `deviceId`); null removes a key.
async fn merge_settings(vm: &crate::vm::AppVm, patch: Map<String, Value>) -> Result<(), String> {
    let path = format!("{VM_PI_HOME}/settings.json");
    let mut cur = read_guest_json(vm, &path).await.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (k, v) in patch {
        if v.is_null() {
            cur.remove(&k);
        } else {
            cur.insert(k, v);
        }
    }
    let text = serde_json::to_string_pretty(&Value::Object(cur)).unwrap_or_else(|_| "{}".into());
    vm.write_file(&path, format!("{text}\n").as_bytes()).await
}

/// Write the kernel-owned agent dir files (every start; they are small).
async fn write_config(vm: &crate::vm::AppVm) -> Result<(), String> {
    vm.write_file(&format!("{VM_PI_HOME}/SYSTEM.md"), system_prompt().as_bytes()).await?;
    vm.write_file(&format!("{VM_PI_HOME}/extensions/openapp.ts"), EXTENSION_TS.as_bytes()).await?;
    vm.write_file(&format!("{VM_PI_HOME}/openapp-login.ts"), LOGIN_TS.as_bytes()).await?;
    let mcp = serde_json::to_string_pretty(&mcp_json()).unwrap_or_default();
    vm.write_file(&format!("{VM_PI_HOME}/mcp.json"), format!("{mcp}\n").as_bytes()).await?;
    merge_settings(vm, base_settings()).await
}

/// Providers with a stored credential in the VM's `auth.json`: (provider, type). Values are never read out.
async fn stored_credentials(vm: &crate::vm::AppVm) -> Vec<(String, String)> {
    let Some(Value::Object(m)) = read_guest_json(vm, &format!("{VM_PI_HOME}/auth.json")).await else { return Vec::new() };
    m.iter().map(|(p, c)| (p.clone(), c.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string())).collect()
}

/// Sign-in choices for the picker card (state `required`).
pub fn methods() -> Value {
    json!([
        { "id": "chatgpt", "label": "Sign in with ChatGPT" },
        { "id": "claude", "label": "Sign in with Claude", "note": "Anthropic may restrict the use of Claude subscriptions in third-party apps." },
        { "id": "apikey", "label": "Use an API key", "providers": API_KEY_PROVIDERS.iter().map(|(id, l)| json!({ "id": id, "label": l })).collect::<Vec<_>>() }
    ])
}

fn required() -> Value {
    json!({ "state": "required", "methods": methods() })
}

/// One stored login as the account card names it ("ChatGPT", "Claude", "OpenAI API key").
fn login_label(p: &str, t: &str) -> String {
    match (p, t) {
        ("openai", "oauth") => "ChatGPT".into(),
        ("anthropic", "oauth") => "Claude".into(),
        (p, "api_key") => format!("{} API key", provider_label(p)),
        (p, _) => provider_label(p),
    }
}

/// The provider's answer means the stored login no longer works (expired, revoked, wrong key): the user must sign in
/// again. Connection and model errors do not match.
pub fn auth_refused(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "401", "unauthorized", "unauthenticated", "invalid_api_key", "invalid api key", "incorrect api key", "authentication",
        "token has expired", "token expired", "token_expired", "invalid_grant", "refresh token", "failed to refresh",
        "sign in again", "log in again", "login again", "re-authenticate", "no api key", "not logged in",
    ]
    .iter()
    .any(|p| e.contains(p))
}

fn provider_label(p: &str) -> String {
    match p {
        "openai" => "OpenAI".into(),
        "anthropic" => "Anthropic".into(),
        other => other.to_string(),
    }
}

/// Sign-in state of the VM Pi → the picker card. `model` = `provider/id` of the active model.
async fn refresh_account(k: &Arc<Kernel>, c: &Arc<PiConn>, model: Option<String>) -> Option<bool> {
    let vm = k.vm()?;
    let creds = stored_credentials(&vm).await;
    let signed = !creds.is_empty();
    let primary = model.as_deref().and_then(|m| m.split('/').next()).and_then(|p| creds.iter().find(|(cp, _)| cp == p)).or(creds.first());
    let note = match primary {
        Some((p, t)) if t == "oauth" && p == "openai" => "Signed in with ChatGPT (in the app VM)".to_string(),
        Some((p, t)) if t == "oauth" && p == "anthropic" => "Signed in with Claude (in the app VM)".to_string(),
        Some((p, t)) if t == "api_key" => format!("API key: {} (in the app VM)", provider_label(p)),
        Some((p, _)) => format!("Signed in: {} (in the app VM)", provider_label(p)),
        None => "Sign in required: Pi in the app VM has its own login".to_string(),
    };
    let active = primary.map(|(p, _)| p.clone());
    let logins: Vec<Value> = creds
        .iter()
        .map(|(p, t)| json!({ "provider": p, "type": t, "label": login_label(p, t), "active": active.as_deref() == Some(p.as_str()) }))
        .collect();
    let accounts = json!({ "logins": logins, "methods": methods() });
    let flow_open = c.login.locked().is_some();
    k.update_harness(&c.harness_id, |o| {
        o.accounts = Some(accounts);
        o.signed_in = Some(signed);
        if signed {
            o.login = None;
            o.note = Some(note);
        } else if !flow_open {
            o.login = Some(required());
            o.note = Some(note);
        }
    });
    k.research.log("kernel", Kind::HarnessAccount, json!({ "harness": c.harness_id, "signed_in": signed, "providers": creds.iter().map(|(p, t)| format!("{p}:{t}")).collect::<Vec<_>>() }));
    Some(signed)
}

fn model_of(state: &Value) -> Option<String> {
    let m = state.get("model")?;
    let (p, id) = (m.get("provider")?.as_str()?, m.get("id")?.as_str()?);
    (p != "unknown" && !p.is_empty()).then(|| format!("{p}/{id}"))
}

// ---------- §23 models ----------

/// Pi level for an effort of the ladder (`none` is Pi's `off`).
fn pi_level(e: &str) -> String {
    if e.eq_ignore_ascii_case("none") { "off".into() } else { e.to_ascii_lowercase() }
}

/// `/openapp-models` reply → catalog entries (`provider/id`). The primary provider comes first. A ChatGPT sign-in
/// without a reachable live list is cross-checked against Codex's cached account list (the same account kind).
/// Catalog-only lists (nothing live) put models with more effort steps first (newer reasoning models), so the
/// fallback default is not simply the alphabetically first one; a refused model is then dropped on its first error.
/// Returns (models, source, error).
pub fn catalog_from_discovery(harness: &str, v: &Value, primary: Option<&str>, codex: &[crate::primitives::models::ModelInfo]) -> (Vec<crate::primitives::models::ModelInfo>, String, Option<String>) {
    use crate::primitives::models::ModelInfo;
    let mut providers: Vec<&Value> = v.get("providers").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    providers.sort_by_key(|p| (p.get("provider").and_then(Value::as_str) != primary) as u8);
    let mut out = Vec::new();
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for p in providers {
        let provider = p.get("provider").and_then(Value::as_str).unwrap_or("");
        let mut source = p.get("source").and_then(Value::as_str).unwrap_or("catalog").to_string();
        if let Some(e) = p.get("error").and_then(Value::as_str) {
            errors.push(format!("{provider}: {e}"));
        }
        let mut models: Vec<Value> = p.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
        let oauth = p.get("auth").and_then(Value::as_str) == Some("oauth");
        let mut codex_order: Vec<&ModelInfo> = Vec::new();
        if provider == "openai" && oauth && source == "catalog" && !codex.is_empty() {
            let filtered: Vec<Value> = codex.iter().filter_map(|cm| models.iter().find(|m| m["id"] == cm.id.as_str()).cloned()).collect();
            if !filtered.is_empty() {
                codex_order = codex.iter().filter(|cm| filtered.iter().any(|m| m["id"] == cm.id.as_str())).collect();
                models = filtered;
                source = "codex-account-list".into();
            }
        }
        sources.push(format!("{provider}:{source}"));
        if source == "catalog" {
            let n = |m: &Value| m.get("levels").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            models.sort_by_key(|m| std::cmp::Reverse(n(m)));
        }
        for m in models {
            let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
            let full = format!("{provider}/{id}");
            let levels: Vec<String> = m.get("levels").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
            let codex_m = codex_order.iter().find(|c| c.id == id);
            let live: Option<Vec<String>> = m
                .get("liveEfforts")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(pi_level).collect())
                .or_else(|| codex_m.map(|c| c.efforts.iter().map(|e| pi_level(e)).collect()));
            let efforts: Vec<String> = match &live {
                Some(l) if levels.iter().any(|x| l.contains(x)) => levels.iter().filter(|x| l.contains(x)).cloned().collect(),
                _ => levels.clone(),
            };
            let default_effort = m
                .get("defaultEffort")
                .and_then(Value::as_str)
                .map(pi_level)
                .or_else(|| codex_m.and_then(|c| c.default_effort.as_deref().map(pi_level)))
                .filter(|d| efforts.contains(d))
                .or_else(|| crate::primitives::models::nearest_effort("medium", &efforts));
            let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false) || codex_m.is_some_and(|c| c.is_default);
            out.push(ModelInfo {
                id: full,
                display_name: codex_m.map(|c| c.display_name.clone()).or_else(|| m.get("name").and_then(Value::as_str).map(String::from)).unwrap_or_else(|| id.to_string()),
                description: m.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()).map(String::from).or_else(|| codex_m.map(|c| c.description.clone())).unwrap_or_default(),
                efforts,
                default_effort,
                hidden: m.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                is_default,
                harness: harness.to_string(),
            });
        }
    }
    let err = (!errors.is_empty()).then(|| errors.join("; "));
    (out, if sources.is_empty() { "none".into() } else { sources.join(" ") }, err)
}

/// §23 discovery: `/openapp-models` (Pi's available models filtered by the account's live list), else Pi's
/// `get_available_models` (catalog only). Returns the number of models.
pub async fn discover_models(k: &Arc<Kernel>) -> Result<usize, String> {
    let c = conn(k).ok_or("Pi is not running")?;
    let _run = c.run_lock.lock().await;
    let (tx, rx) = oneshot::channel();
    *c.models_reply.locked() = Some(tx);
    let reply = match c.request(json!({ "type": "prompt", "message": "/openapp-models" }), Duration::from_secs(60)).await {
        Ok(_) => tokio::time::timeout(Duration::from_secs(90), rx).await.ok().and_then(Result::ok),
        Err(e) => {
            log::warn!("pi /openapp-models: {e}");
            None
        }
    };
    c.models_reply.locked().take();
    let primary = match k.vm() {
        Some(vm) if !c.host => read_guest_json(&vm, &format!("{VM_PI_HOME}/settings.json")).await.and_then(|s| s.get("defaultProvider").and_then(Value::as_str).map(String::from)),
        _ => None,
    };
    let codex = crate::primitives::models::cached_catalog(k, "codex");
    let (models, source, error) = match reply {
        Some(v) => catalog_from_discovery(&c.harness_id, &v, primary.as_deref(), &codex),
        None => {
            // catalog only: what Pi knows for the signed-in providers
            let list = c.request(json!({ "type": "get_available_models" }), Duration::from_secs(30)).await?;
            let providers: Vec<Value> = {
                let models: Vec<Value> = list.get("models").and_then(Value::as_array).cloned().unwrap_or_default();
                let mut by: Vec<(String, Vec<Value>)> = Vec::new();
                for m in models {
                    let p = m.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                    let levels = levels_of(&m);
                    let entry = json!({ "id": m["id"], "name": m["name"], "levels": levels });
                    match by.iter_mut().find(|(q, _)| *q == p) {
                        Some((_, v)) => v.push(entry),
                        None => by.push((p, vec![entry])),
                    }
                }
                by.into_iter().map(|(p, ms)| json!({ "provider": p, "source": "catalog", "models": ms })).collect()
            };
            let (m, s, _) = catalog_from_discovery(&c.harness_id, &json!({ "providers": providers }), primary.as_deref(), &codex);
            (m, s, Some("the OpenApp model listing did not answer; using Pi's catalog".to_string()))
        }
    };
    drop(_run);
    let n = models.len();
    crate::primitives::models::set_catalog(k, &c.harness_id, models, &source, error);
    Ok(n)
}

/// pi-ai `getSupportedThinkingLevels` for a `get_available_models` entry.
fn levels_of(m: &Value) -> Vec<String> {
    if m.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return vec!["off".into()];
    }
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .filter(|l| {
            let mapped = m.pointer(&format!("/thinkingLevelMap/{l}"));
            match mapped {
                Some(Value::Null) => false,
                None if **l == "xhigh" || **l == "max" => false,
                _ => true,
            }
        })
        .map(|l| l.to_string())
        .collect()
}

/// Make Pi run on the resolved model/effort of this session and mode (`set_model` + `set_thinking_level`), and keep
/// it as Pi's startup default (agent modules start Pi with it). None = nothing discovered (Pi's own default).
async fn apply_choice(k: &Arc<Kernel>, c: &Arc<PiConn>, oa: Option<&str>, mode: &str) -> Option<crate::primitives::models::Resolved> {
    let r = crate::primitives::models::resolve(k, &c.harness_id, mode, oa)?;
    let (provider, id) = r.model.id.split_once('/')?;
    let st = c.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await.ok();
    let cur = st.as_ref().and_then(model_of);
    let cur_level = st.as_ref().and_then(|s| s.get("thinkingLevel").and_then(Value::as_str)).map(String::from);
    let switched = cur.as_deref() != Some(r.model.id.as_str());
    if switched
        && let Err(e) = c.request(json!({ "type": "set_model", "provider": provider, "modelId": id }), Duration::from_secs(30)).await {
            log::warn!("pi set_model {}: {e}", r.model.id);
            return Some(r);
        }
    let level = r.effort.as_deref().map(pi_level);
    if let Some(l) = &level
        && (switched || cur_level.as_deref() != Some(l.as_str()))
            && let Err(e) = c.request(json!({ "type": "set_thinking_level", "level": l }), Duration::from_secs(30)).await {
                log::warn!("pi set_thinking_level {l}: {e}");
            }
    if switched || level.is_some() {
        k.research.log("kernel", Kind::TurnModel, json!({ "session": oa, "harness": c.harness_id, "model": r.model.id, "effort": level, "tier": r.tier, "switched": switched }));
    }
    let key = format!("{}:{}", r.model.id, level.clone().unwrap_or_default());
    let stale = c.settings_model.locked().as_deref() != Some(key.as_str());
    if stale && r.tier == "default" && !c.host
        && let Some(vm) = k.vm() {
            let mut m = Map::new();
            m.insert("defaultProvider".into(), json!(provider));
            m.insert("defaultModel".into(), json!(id));
            m.insert("defaultThinkingLevel".into(), level.clone().map(Value::String).unwrap_or(Value::Null));
            match merge_settings(&vm, m).await {
                Ok(()) => *c.settings_model.locked() = Some(key),
                Err(e) => log::warn!("pi settings: {e}"),
            }
        }
    Some(r)
}

/// §23 escalation in the middle of a Pi turn: switch now (Pi uses the model from its next request).
pub async fn apply_now(k: &Arc<Kernel>, session: &str) -> Result<(), String> {
    let c = conn(k).ok_or("Pi is not running")?;
    let mode = k.session_rec(session).map(|r| r.mode).unwrap_or_else(|| "chat".into());
    apply_choice(k, &c, Some(session), &mode).await.map(|_| ()).ok_or_else(|| "no model to apply".to_string())
}

// ---------- process ----------

/// A started Pi RPC process: pipes, kill handle, exit waiter.
struct Launched {
    stdin: crate::vm::BoxWrite,
    stdout: crate::vm::BoxRead,
    stderr: crate::vm::BoxRead,
    killer: Killer,
    waiter: std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>,
    where_: String,
}

/// Pi in the app VM: install (once), kernel-owned config, `pi --mode rpc`.
async fn launch_vm(k: &Arc<Kernel>, harness_id: &str, token: &str) -> Result<Launched, String> {
    k.update_harness(harness_id, |o| o.note = Some("Starting the app VM…".into()));
    let vm = k.ensure_vm().await?;
    let bin = install_in_vm(k, &vm, harness_id).await?;
    write_config(&vm).await?;
    let mut child = vm.spawn(&bin, &["--mode".into(), "rpc".into()], &guest_env(Some(token)), Some(GUEST_WS)).await?;
    let stdin = child.take_stdin();
    let stdout = child.take_stdout();
    let stderr = child.stderr.take().expect("guest stderr");
    let killer = Killer::Guest(child.killer());
    Ok(Launched {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        killer,
        waiter: Box::pin(async move {
            let i = child.wait_info().await;
            match i.failed {
                Some(f) => format!("failed to start: {f}"),
                None => format!("exit status: {}", i.code),
            }
        }),
        where_: format!("app VM {}", vm.name),
    })
}

/// Test transport (`OPENAPP_PI_CMD`): a host command that speaks Pi RPC (e.g. a fake Pi). No VM, no config.
fn launch_host(k: &Arc<Kernel>, cmd: &str, token: &str) -> Result<Launched, String> {
    let mut c = harness::host_command(k, cmd);
    c.env(crate::codex::TOKEN_ENV, token);
    let mut child = c.spawn().map_err(|e| format!("could not start {TEST_CMD_ENV}: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take().expect("stdin"), child.stdout.take().expect("stdout"), child.stderr.take().expect("stderr"));
    let pid = child.id();
    Ok(Launched {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        killer: Killer::Host(pid),
        waiter: Box::pin(async move { child.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()) }),
        where_: "host test process".into(),
    })
}

/// Start `pi --mode rpc` in the app VM and wait until it answers `get_state`.
pub async fn run(k: Arc<Kernel>, spec: harness::HarnessSpec) {
    k.set_agent("starting", None);
    let Some(ep) = k.mcp.get() else {
        k.set_agent("error", Some("kernel MCP server is not running".into()));
        return;
    };
    let log_path = harness::agent_log(&k, "pi --mode rpc (app VM)");
    // the host test transport runs outside the VM boundary: debug builds only
    let test_cmd = std::env::var(TEST_CMD_ENV).ok().filter(|s| cfg!(debug_assertions) && !s.trim().is_empty());
    let launched = match &test_cmd {
        Some(cmd) => launch_host(&k, cmd, &ep.token),
        None => launch_vm(&k, &spec.id, &ep.token).await,
    };
    let Launched { stdin, stdout, stderr, killer, waiter, where_ } = match launched {
        Ok(v) => v,
        Err(e) => {
            k.update_harness(&spec.id, |o| o.note = Some(e.lines().next().unwrap_or("").to_string()));
            k.set_agent("error", Some(e));
            return;
        }
    };
    let host = test_cmd.is_some();
    let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
    modules::pump_stderr(stderr, log_path, tail.clone());
    let c = Arc::new(PiConn {
        peer: Peer::new(rpc::writer(stdin)),
        killer,
        host,
        models_reply: Mutex::new(None),
        settings_model: Mutex::new(None),
        harness_id: spec.id.clone(),
        tail,
        run_lock: tokio::sync::Mutex::new(()),
        current: Mutex::new(None),
        turn: Mutex::new(None),
        login: Mutex::new(None),
        notices: Mutex::new(VecDeque::new()),
    });
    *k.pi.locked() = Some(c.clone());
    log::info!("pi ({where_}): rpc mode, agent dir {VM_PI_HOME}");

    {
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move { rpc::read_lines(stdout, &c.tail, |v| handle_record(&k, &c, v)).await });
    }
    {
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move {
            let status = waiter.await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.peer.clear();
            c.turn.locked().take(); // drops the turn waiter
            if let Some(f) = c.login.locked().take() {
                f.killer.kill();
            }
            let is_current = conn(&k).is_some_and(|cur| Arc::ptr_eq(&cur, &c));
            if is_current {
                *k.pi.locked() = None;
            }
            harness::report_exit(&k, is_current, "pi", &status, &c.tail);
        });
    }

    match c.request(json!({ "type": "get_state" }), Duration::from_secs(120)).await {
        Ok(state) => {
            let signed = if host { model_of(&state).is_some() } else { refresh_account(&k, &c, model_of(&state)).await == Some(true) };
            // §23: what the account offers, then the user's choice (else the discovered default)
            let model = if signed {
                if let Err(e) = discover_models(&k).await {
                    log::warn!("pi model discovery: {e}");
                }
                apply_choice(&k, &c, None, "chat").await.map(|r| r.model.id)
            } else {
                None
            };
            k.research.log(
                "kernel",
                Kind::HarnessConfig,
                json!({ "harness": spec.id, "runtime": if host { "host-test" } else { "vm" }, "where": where_, "version": PI_VERSION, "agent_dir": VM_PI_HOME, "mode": "rpc", "approval": "none (VM boundary)", "model": model, "egress": "proxy+consent" }),
            );
            k.set_agent_name(&spec.name);
            k.set_agent("ready", None);
            // §23: what an account offers changes (new releases, plan changes): discover again every 6 hours
            if signed {
                let (k, c) = (k.clone(), c.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(MODELS_REFRESH).await;
                        if !conn(&k).is_some_and(|cur| Arc::ptr_eq(&cur, &c)) {
                            return;
                        }
                        if let Err(e) = discover_models(&k).await {
                            log::warn!("pi model discovery: {e}");
                        }
                    }
                });
            }
        }
        Err(e) => {
            let last = modules::tail_lines(&c.tail, 10).join("\n");
            k.set_agent("error", Some(format!("pi did not answer get_state: {e}\n{last}")));
        }
    }
}

pub fn kill(k: &Kernel) {
    if let Some(c) = conn(k) {
        c.terminate();
    }
}

pub fn forget_session(k: &Kernel, oa: &str) {
    if let Some(c) = conn(k) {
        let mut cur = c.current.locked();
        if cur.as_deref() == Some(oa) {
            *cur = None;
        }
    }
}

pub fn cancel(k: &Kernel) {
    let oa = k.active_session();
    let Some(c) = conn(k) else { return };
    if c.turn_oa().as_deref() != Some(oa.as_str()) {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = c.request(json!({ "type": "abort" }), Duration::from_secs(30)).await {
            log::warn!("pi abort failed: {e}");
        }
    });
}

/// Load the Pi session of an OpenApp session: current, `switch_session` (stored file), else `new_session`.
async fn ensure_session(k: &Arc<Kernel>, c: &Arc<PiConn>, oa: &str) -> Result<(), String> {
    if c.current.locked().as_deref() == Some(oa) {
        return Ok(());
    }
    if let Some(path) = k.agent_session_id(oa, &c.harness_id) {
        match c.request(json!({ "type": "switch_session", "sessionPath": path }), Duration::from_secs(60)).await {
            Ok(d) if d.get("cancelled").and_then(Value::as_bool) != Some(true) => {
                *c.current.locked() = Some(oa.to_string());
                return Ok(());
            }
            Ok(_) => log::warn!("pi switch_session {path} was cancelled; starting a new session"),
            Err(e) => log::warn!("pi switch_session {path} failed ({e}); starting a new session"),
        }
    }
    let d = c.request(json!({ "type": "new_session" }), Duration::from_secs(60)).await?;
    if d.get("cancelled").and_then(Value::as_bool) == Some(true) {
        return Err("Pi did not start a new session".into());
    }
    let st = c.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await?;
    *c.current.locked() = Some(oa.to_string());
    k.set_agent_session_id(oa, &c.harness_id, st.get("sessionFile").and_then(Value::as_str).map(String::from));
    Ok(())
}

pub async fn run_turn(k: Arc<Kernel>, oa: String, text: String, build: bool) {
    let lock = k.turn_locks.locked().entry(oa.clone()).or_default().clone();
    let _turn = lock.lock().await;
    let _tg = crate::kernel::TurnGuard::begin(&k, &oa);
    if let Err(msg) = harness::wait_ready(&k).await {
        k.system_message_in(&oa, "error", &msg);
        k.research.log("kernel", Kind::TurnEnd, json!({ "session": oa, "error": msg }));
        return;
    }
    let Some(c) = conn(&k) else {
        k.system_message_in(&oa, "error", "Pi is not connected.");
        return;
    };
    if k.harness_overlay(&c.harness_id).signed_in == Some(false) {
        k.system_message_in(&oa, "error", "Pi in the app VM is not signed in yet. Use \"Sign in\" on the Pi card (type /provider to see it).");
        return;
    }
    turn_started(&k);
    let t0 = Instant::now();
    let mut chosen = None;
    let result = async {
        let _run = c.run_lock.lock().await;
        ensure_session(&k, &c, &oa).await?;
        // §23: after the session (switch_session restores the session's own model)
        chosen = apply_choice(&k, &c, Some(&oa), if build { "build" } else { "chat" }).await;
        let (tx, rx) = oneshot::channel();
        *c.turn.locked() = Some(PTurn { oa: oa.clone(), done: Some(tx), ..Default::default() });
        let r = c.request(json!({ "type": "prompt", "message": turn_message(&text, build) }), Duration::from_secs(120)).await?;
        if r.get("disposition").and_then(Value::as_str) == Some("handled") {
            return Ok(json!({ "aborted": false, "handled": true }));
        }
        rx.await.map_err(|_| "Pi stopped during the turn".to_string())
    }
    .await;
    let ms = t0.elapsed().as_millis() as u64;
    let turn = c.turn.locked().take();
    let (full, error) = turn.map(|t| (t.text, t.error)).unwrap_or_default();
    if !full.is_empty() {
        k.research.log("agent", Kind::AgentMessage, json!({ "session": oa, "text": full }));
    }
    cancel_open_permissions(&k, Some(&oa));
    match result {
        Ok(v) => {
            let aborted = v.get("aborted").and_then(Value::as_bool) == Some(true);
            let stop = if aborted { "aborted" } else if error.is_some() { "error" } else { "completed" };
            k.research.log(crate::research::turn_end_actor(stop), Kind::TurnEnd, json!({ "session": oa, "stop_reason": stop, "error": error, "ms": ms }));
            if aborted {
                k.system_message_in(&oa, "system", "Stopped.");
            } else if let Some(e) = error {
                k.system_message_in(&oa, "error", &format!("The turn failed: {e}"));
                // the stored login stopped working: the sign-in card opens instead of the composer
                if auth_refused(&e) {
                    let who = chosen.as_ref().and_then(|r| r.model.id.split('/').next().map(provider_label)).unwrap_or_else(|| "The provider".into());
                    k.update_harness(&c.harness_id, |o| {
                        o.signed_in = Some(false);
                        o.login = Some(json!({ "state": "failed", "message": format!("{who} refused the sign-in ({}). Sign in again, or use another login.", crate::util::short(&e, 160)), "methods": methods() }));
                        o.note = Some("Sign-in expired: sign in again".into());
                    });
                    k.research.log("kernel", Kind::HarnessAccount, json!({ "harness": c.harness_id, "signed_in": false, "reason": "refused", "error": e }));
                }
                // §23: the provider refused the model for this account → drop it, fall back
                if let Some(msg) = chosen.as_ref().and_then(|r: &crate::primitives::models::Resolved| crate::primitives::models::reject(&k, &c.harness_id, &r.model.id, &e)) {
                    k.system_message_in(&oa, "system", &msg);
                }
            }
        }
        Err(e) => {
            k.research.log("agent", Kind::TurnEnd, json!({ "session": oa, "error": e, "ms": ms }));
            k.system_message_in(&oa, "error", &format!("Pi request failed: {e}"));
        }
    }
    crate::primitives::models::end_turn(&k, &oa);
    k.touch_session(&oa);
    turn_finished(&k);
}

// ---------- records from Pi ----------

fn handle_record(k: &Arc<Kernel>, c: &Arc<PiConn>, v: Value) {
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "response" => {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                c.peer.resolve(id, v.clone());
            } else if v.get("success").and_then(Value::as_bool) == Some(false) {
                modules::push_tail(&c.tail, format!("[response] {v}"));
            }
        }
        "extension_ui_request" => handle_ui_request(k, c, v),
        "extension_error" => {
            let msg = format!("[extension_error] {} {}: {}", v["extensionPath"].as_str().unwrap_or(""), v["event"].as_str().unwrap_or(""), v["error"].as_str().unwrap_or(""));
            log::warn!("pi {msg}");
            modules::push_tail(&c.tail, msg);
        }
        ty => handle_event(k, c, ty, &v),
    }
}

/// Chip title for a Pi tool (None = no chip).
fn tool_title(name: &str, args: &Value) -> Option<String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    Some(match name {
        "ask_user" => return None,
        "bash" => format!("Run {}", short(s("command"), 80)),
        "read" => format!("Read {}", base(s("path"))),
        "edit" => format!("Edit {}", base(s("path"))),
        "write" => format!("Write {}", base(s("path"))),
        "grep" => format!("Search {}", short(s("pattern"), 60)),
        "find" => format!("Find {}", short(s("pattern"), 60)),
        "ls" => format!("List {}", base(s("path"))),
        n if n.starts_with("mcp__") => {
            let mut parts = n.splitn(3, "__").skip(1);
            let (server, tool) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            format!("{server}.{tool}")
        }
        n => format!("Tool {n}"),
    })
}

fn result_text(r: &Value) -> String {
    r.get("content")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// Error text of a failed non-bash tool result (text blocks, else a plain string / `error` field, else the JSON).
fn failure_text(r: &Value) -> String {
    let t = result_text(r);
    if !t.trim().is_empty() {
        return t;
    }
    match r {
        Value::String(s) => s.clone(),
        Value::Null => "the tool failed without a message".into(),
        _ => r.get("error").and_then(|e| e.as_str().map(String::from).or_else(|| e.get("message").and_then(Value::as_str).map(String::from))).unwrap_or_else(|| r.to_string()),
    }
}

fn handle_event(k: &Arc<Kernel>, c: &Arc<PiConn>, ty: &str, v: &Value) {
    let mut guard = c.turn.locked();
    let Some(t) = guard.as_mut() else {
        if matches!(ty, "auto_retry_start" | "compaction_start" | "compaction_end") {
            modules::push_tail(&c.tail, format!("[{ty}] {v}"));
        }
        return;
    };
    let oa = t.oa.clone();
    match ty {
        "message_start" => {
            if v.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                t.blocks.clear();
            }
        }
        "message_update" => {
            let e = &v["assistantMessageEvent"];
            let idx = e.get("contentIndex").and_then(Value::as_u64).unwrap_or(0);
            match e.get("type").and_then(Value::as_str).unwrap_or("") {
                et @ ("text_delta" | "thinking_delta") => {
                    let Some(delta) = e.get("delta").and_then(Value::as_str) else { return };
                    let (kind, key) = if et == "text_delta" { ("agent", format!("text:{idx}")) } else { ("thought", format!("thinking:{idx}")) };
                    if kind == "agent" {
                        t.text.push_str(delta);
                    }
                    match t.blocks.get(&key).cloned() {
                        Some(chat) => {
                            drop(guard);
                            k.chat_append_text_in(&oa, &chat, delta);
                        }
                        None => {
                            let chat = k.chat_append_to(&oa, json!({ "kind": kind, "text": delta }));
                            t.blocks.insert(key, chat);
                        }
                    }
                }
                et @ ("text_end" | "thinking_end") => {
                    let key = if et == "text_end" { format!("text:{idx}") } else { format!("thinking:{idx}") };
                    let content = e.get("content").and_then(Value::as_str).unwrap_or("");
                    if let (Some(chat), false) = (t.blocks.get(&key).cloned(), content.is_empty()) {
                        drop(guard);
                        let mut patch = Map::new();
                        patch.insert("text".into(), json!(content));
                        k.chat_update_in(&oa, &chat, patch);
                    }
                }
                _ => {}
            }
        }
        "message_end" => {
            let m = &v["message"];
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                return;
            }
            match m.get("stopReason").and_then(Value::as_str) {
                Some("error") => t.error = Some(m.get("errorMessage").and_then(Value::as_str).unwrap_or("the provider returned an error").to_string()),
                Some("aborted") => {}
                _ => t.error = None,
            }
            // providers that do not stream: show the final text once
            if !t.blocks.keys().any(|k| k.starts_with("text:")) {
                let text: Vec<&str> = m
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|b| b["type"] == "text").filter_map(|b| b.get("text").and_then(Value::as_str)).collect())
                    .unwrap_or_default();
                let text = text.join("\n\n");
                if !text.trim().is_empty() {
                    t.text.push_str(&text);
                    let chat = k.chat_append_to(&oa, json!({ "kind": "agent", "text": text }));
                    t.blocks.insert("text:0".into(), chat);
                }
            }
        }
        "tool_execution_start" => {
            let (Some(id), Some(name)) = (v.get("toolCallId").and_then(Value::as_str), v.get("toolName").and_then(Value::as_str)) else { return };
            let args = v.get("args").cloned().unwrap_or(Value::Null);
            let Some(title) = tool_title(name, &args) else { return };
            let mut chip = json!({ "kind": "tool", "title": title, "status": "in_progress" });
            if name == "bash" {
                chip["command"] = args["command"].clone();
                chip["cwd"] = json!(GUEST_WS);
                chip["output"] = json!("");
            }
            let chat = k.chat_append_to(&oa, chip);
            t.tools.insert(id.to_string(), (chat, Instant::now()));
            k.research.log("agent", Kind::ToolCall, json!({ "session": oa, "id": id, "type": name, "title": title, "status": "in_progress" }));
        }
        "tool_execution_update" => {
            let Some(id) = v.get("toolCallId").and_then(Value::as_str) else { return };
            if v.get("toolName").and_then(Value::as_str) != Some("bash") {
                return;
            }
            let Some((chat, last)) = t.tools.get_mut(id) else { return };
            if last.elapsed() < Duration::from_millis(250) {
                return;
            }
            *last = Instant::now();
            let chat = chat.clone();
            let out = tail_chars(&result_text(&v["partialResult"]), OUTPUT_TAIL);
            drop(guard);
            let mut patch = Map::new();
            patch.insert("output".into(), json!(out));
            k.chat_update_in(&oa, &chat, patch);
        }
        "tool_execution_end" => {
            let Some(id) = v.get("toolCallId").and_then(Value::as_str) else { return };
            let name = v.get("toolName").and_then(Value::as_str).unwrap_or("");
            let Some((chat, _)) = t.tools.remove(id) else { return };
            drop(guard);
            let failed = v.get("isError").and_then(Value::as_bool) == Some(true);
            let status = if failed { "failed" } else { "completed" };
            let r = &v["result"];
            let mut patch = Map::new();
            patch.insert("status".into(), json!(status));
            if name == "bash" {
                patch.insert("output".into(), json!(tail_chars(&result_text(r), OUTPUT_TAIL)));
                let code = r.pointer("/structuredContent/exit_code").cloned().unwrap_or(if failed { Value::Null } else { json!(0) });
                patch.insert("exitCode".into(), code);
            } else if failed {
                // find / grep / read / edit / write / ls / MCP tools: the error text explains the failed chip
                patch.insert("output".into(), json!(tail_chars(&failure_text(r), OUTPUT_TAIL)));
            }
            k.chat_update_in(&oa, &chat, patch);
            let err = failed.then(|| short(&result_text(r), 300));
            k.research.log("agent", Kind::ToolCall, json!({ "session": oa, "id": id, "type": name, "status": status, "error": err, "ms": v.get("durationMs") }));
        }
        "agent_settled" => {
            if let Some(tx) = t.done.take() {
                let _ = tx.send(json!({ "aborted": v.get("aborted").and_then(Value::as_bool).unwrap_or(false) }));
            }
        }
        "auto_retry_start" | "auto_retry_end" | "compaction_start" | "compaction_end" => {
            modules::push_tail(&c.tail, format!("[{ty}] {}", short(&v.to_string(), 300)));
        }
        _ => {}
    }
}

/// Extension UI (§12.4): dialogs become question / permission cards; fire-and-forget calls are noted.
fn handle_ui_request(k: &Arc<Kernel>, c: &Arc<PiConn>, v: Value) {
    let Some(id) = v.get("id").and_then(Value::as_str).map(String::from) else { return };
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let title = v.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let oa = c.turn_oa().unwrap_or_else(|| k.active_session());
    let timeout = v.get("timeout").and_then(Value::as_u64).map(Duration::from_millis);
    match method {
        "notify" if v.get("message").and_then(Value::as_str).is_some_and(|m| m.starts_with(MODELS_MARK)) => {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
            let parsed = serde_json::from_str::<Value>(&msg[MODELS_MARK.len()..]).unwrap_or(Value::Null);
            modules::push_tail(&c.tail, format!("[models] {} provider(s)", parsed["providers"].as_array().map(Vec::len).unwrap_or(0)));
            if let Some(tx) = c.models_reply.locked().take() {
                let _ = tx.send(parsed);
            }
        }
        "notify" => {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            modules::push_tail(&c.tail, format!("[notify {}] {msg}", v["notifyType"].as_str().unwrap_or("info")));
            let mut n = c.notices.locked();
            n.push_back(msg);
            while n.len() > 50 {
                n.pop_front();
            }
        }
        "select" | "input" | "editor" => {
            let (questions, single) = if let Some(payload) = title.strip_prefix(ASK_MARK) {
                let qs = serde_json::from_str::<Value>(payload).ok().and_then(|p| p.get("questions").and_then(Value::as_array).cloned()).unwrap_or_default();
                (qs, false)
            } else {
                let options: Option<Vec<Value>> = (method == "select").then(|| {
                    v.get("options").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|l| json!({ "label": l, "description": "" })).collect()).unwrap_or_default()
                });
                let q = json!({
                    "id": "q", "header": null,
                    "question": if title.is_empty() { v.get("message").and_then(Value::as_str).unwrap_or("Pi has a question").to_string() } else { title.clone() },
                    "options": options, "isOther": method != "select", "isSecret": false
                });
                (vec![q], true)
            };
            if questions.is_empty() {
                c.send(json!({ "type": "extension_ui_response", "id": id, "cancelled": true }));
                return;
            }
            let (item, rx) = k.ask_questions(&oa, questions, "pi");
            let c2 = c.clone();
            tokio::spawn(async move {
                let resp = match rx.await.unwrap_or(None) {
                    Some(answers) if single => match answers.get("q").and_then(|a| a.as_array().and_then(|l| l.first()).or(Some(a))).and_then(Value::as_str) {
                        Some(s) => json!({ "type": "extension_ui_response", "id": id, "value": s }),
                        None => json!({ "type": "extension_ui_response", "id": id, "cancelled": true }),
                    },
                    Some(answers) => json!({ "type": "extension_ui_response", "id": id, "value": Value::Object(answers).to_string() }),
                    None => json!({ "type": "extension_ui_response", "id": id, "cancelled": true }),
                };
                c2.send(resp);
            });
            if let Some(d) = timeout {
                let k = k.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(d).await;
                    let _ = k.answer_question(&item, None);
                });
            }
        }
        "confirm" => {
            let text = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            let options = json!([
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]);
            let (item, rx) = crate::consent::open(k, &oa, json!({ "title": if title.is_empty() { "Pi asks".to_string() } else { title.clone() }, "text": text, "options": options }), false);
            k.research.log("agent", Kind::Permission, json!({ "phase": "shown", "session": oa, "id": item, "method": "pi/confirm", "title": title }));
            let (k2, c2, item2) = (k.clone(), c.clone(), item.clone());
            let started = Instant::now();
            tokio::spawn(async move {
                let choice = rx.await.unwrap_or(None);
                c2.send(json!({ "type": "extension_ui_response", "id": id, "confirmed": choice.as_deref() == Some("allow") }));
                crate::consent::close(&k2, &oa, &item2, &choice);
                k2.research.log("user", Kind::Permission, json!({ "phase": "decided", "session": oa, "id": item2, "option": choice, "ms": started.elapsed().as_millis() as u64 }));
            });
            if let Some(d) = timeout {
                let k = k.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(d).await;
                    let _ = k.respond_permission(&item, None);
                });
            }
        }
        // setStatus, setWidget, setTitle, set_editor_text: not shown
        _ => {}
    }
}

// ---------- sign-in (§17) ----------

/// Start a Pi OAuth sign-in in the VM: `method` = `chatgpt` | `claude`. The helper's callback
/// server runs in the guest; the kernel forwards the host port and opens the URL in the host
/// browser. A pasted redirect URL (`login_submit` kind `code`) also completes it.
pub async fn login(k: &Arc<Kernel>, method: &str, open_browser: bool) -> Result<Value, String> {
    let (mid, provider, label, port) = *OAUTH.iter().find(|(m, ..)| *m == method).ok_or_else(|| format!("unknown sign-in method \"{method}\" (chatgpt, claude)"))?;
    harness::wait_ready(k).await?;
    let c = conn(k).ok_or("Pi is not running")?;
    if let Some(prev) = c.login.locked().take() {
        prev.killer.kill();
    }
    k.update_harness(&c.harness_id, |o| {
        o.login = Some(json!({ "state": "starting", "method": mid }));
        o.note = Some("Starting sign-in…".into());
    });
    k.research.log("user", Kind::HarnessLogin, json!({ "phase": "start", "harness": c.harness_id, "method": mid, "provider": provider }));
    let vm = k.vm().ok_or("the app VM is not running")?;
    let forward = match vm.forward_host_port(port).await {
        Ok(f) => Some(f),
        Err(e) => {
            log::warn!("pi sign-in: {e}; the pasted redirect URL still works");
            None
        }
    };
    let mut env = guest_env(None);
    env.push(("OPENAPP_PI_PKG".into(), pkg_dir()));
    let mut child = vm.spawn("node", &[format!("{VM_PI_HOME}/openapp-login.ts"), provider.to_string()], &env, Some(GUEST_WS)).await?;
    let mut stdin = child.take_stdin();
    let stdout = child.take_stdout();
    if let Some(mut se) = child.stderr.take() {
        let tail = c.tail.clone();
        tokio::spawn(async move {
            let mut s = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut se, &mut s).await;
            for l in s.lines().filter(|l| !l.trim().is_empty()).take(20) {
                modules::push_tail(&tail, format!("[login] {l}"));
            }
        });
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });
    let killer = child.killer();
    *c.login.locked() =
        Some(LoginFlow { provider: provider.into(), method: mid.into(), tx, prompt: None, killer: killer.clone(), _forward: forward });
    {
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    on_login_event(&k, &c, &v, label, port, open_browser).await;
                }
            }
            let _ = child.wait().await;
        });
    }
    {
        // a sign-in still open after 15 minutes is cancelled
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(900)).await;
            let still = c.login.locked().as_ref().is_some_and(|f| f.method == mid);
            if still {
                if let Some(f) = c.login.locked().take() {
                    f.killer.kill();
                }
                k.update_harness(&c.harness_id, |o| {
                    o.login = Some(json!({ "state": "failed", "message": "Sign-in timed out", "methods": methods() }));
                    o.note = Some("Sign-in timed out: try again".into());
                });
            }
        });
    }
    Ok(json!({ "method": mid, "provider": provider, "port": port }))
}

async fn on_login_event(k: &Arc<Kernel>, c: &Arc<PiConn>, v: &Value, label: &str, port: u16, open_browser: bool) {
    let send = |m: Value| {
        if let Some(f) = c.login.locked().as_ref() {
            let _ = f.tx.send(m.to_string());
        }
    };
    match v.get("event").and_then(Value::as_str).unwrap_or("") {
        "auth_url" => {
            let url = v.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let method = c.login.locked().as_ref().map(|f| f.method.clone());
            k.update_harness(&c.harness_id, |o| {
                o.login = Some(json!({ "state": "browser", "url": url, "port": port, "paste": true, "method": method }));
                o.note = Some("Finish signing in in your browser…".into());
            });
            if open_browser && let Err(e) = crate::kernel::open_url(&url) {
                log::warn!("pi sign-in: could not open the browser: {e}");
            }
        }
        "device_code" => {
            let url = v.get("verificationUri").and_then(Value::as_str).unwrap_or("").to_string();
            let code = v.get("userCode").and_then(Value::as_str).unwrap_or("").to_string();
            k.update_harness(&c.harness_id, |o| {
                o.login = Some(json!({ "state": "device", "url": url, "code": code }));
                o.note = Some(format!("Open {url} and enter the code {code}"));
            });
        }
        "prompt" => {
            let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
            match v.get("type").and_then(Value::as_str).unwrap_or("") {
                // e.g. Anthropic's "browser" vs "copy code": the first option is the browser login
                "select" => {
                    let first = v.pointer("/options/0/id").and_then(Value::as_str).unwrap_or("");
                    send(json!({ "id": id, "value": first }));
                }
                "manual_code" => {
                    if let Some(f) = c.login.locked().as_mut() {
                        f.prompt = Some(id);
                    }
                }
                _ => send(json!({ "id": id, "cancel": true })),
            }
        }
        "prompt_closed" => {
            let id = v.get("id").and_then(Value::as_u64);
            if let Some(f) = c.login.locked().as_mut()
                && f.prompt == id {
                    f.prompt = None;
                }
        }
        "progress" | "info" => {
            if let Some(m) = v.get("message").and_then(Value::as_str) {
                let m = m.to_string();
                k.update_harness(&c.harness_id, |o| o.note = Some(m));
            }
        }
        "done" => {
            let flow = c.login.locked().take();
            let ok = v.get("ok").and_then(Value::as_bool) == Some(true);
            let err = v.get("error").and_then(Value::as_str).map(String::from);
            k.research.log("user", Kind::HarnessLogin, json!({ "phase": "completed", "harness": c.harness_id, "success": ok, "error": err }));
            let Some(flow) = flow else { return }; // cancelled by the user
            if ok {
                finish_sign_in(k, &flow.provider, &format!("Pi in the app VM is signed in with {label}.")).await;
            } else {
                let msg = err.unwrap_or_else(|| "sign-in did not complete".into());
                k.update_harness(&c.harness_id, |o| {
                    o.login = Some(json!({ "state": "failed", "message": msg, "methods": methods() }));
                    o.note = Some(format!("Sign-in failed: {msg}"));
                });
            }
        }
        _ => {}
    }
}

/// After a credential is stored: make the provider the default and restart Pi so it loads it.
async fn finish_sign_in(k: &Arc<Kernel>, provider: &str, msg: &str) {
    if let Some(vm) = k.vm() {
        let mut m = Map::new();
        m.insert("defaultProvider".into(), json!(provider));
        // §23: no model literal; the next start discovers what this account offers
        m.insert("defaultModel".into(), Value::Null);
        m.insert("defaultThinkingLevel".into(), Value::Null);
        if let Err(e) = merge_settings(&vm, m).await {
            log::warn!("pi settings: {e}");
        }
    }
    k.system_message("system", msg);
    if let Some(c) = conn(k) {
        crate::primitives::models::announce_after_sign_in(k, &c.harness_id);
    }
    harness::restart(k);
}

/// Answer the open sign-in step: `kind` = `code` (pasted redirect URL / code) or `apikey`
/// (`provider` + key, written to the VM's auth.json). Values are never logged.
pub async fn login_submit(k: &Arc<Kernel>, kind: &str, value: &str, provider: Option<&str>) -> Result<(), String> {
    let c = conn(k).ok_or("Pi is not running")?;
    let value = value.trim();
    if value.is_empty() {
        return Err("nothing was entered".into());
    }
    match kind {
        "code" => {
            let g = c.login.locked();
            let f = g.as_ref().ok_or("no sign-in is waiting for a code")?;
            let id = f.prompt.ok_or("this sign-in step does not take a pasted code")?;
            f.tx.send(json!({ "id": id, "value": value }).to_string()).map_err(|_| "the sign-in helper stopped".to_string())?;
            drop(g);
            k.research.log("user", Kind::HarnessLogin, json!({ "phase": "pasted", "harness": c.harness_id }));
            k.update_harness(&c.harness_id, |o| o.note = Some("Checking the sign-in…".into()));
            Ok(())
        }
        "apikey" => {
            let p = provider.ok_or("choose a provider for the API key")?;
            let (pid, label) = *API_KEY_PROVIDERS.iter().find(|(id, _)| *id == p).ok_or_else(|| format!("unknown provider \"{p}\""))?;
            let vm = k.vm().ok_or("the app VM is not running")?;
            let path = format!("{VM_PI_HOME}/auth.json");
            let mut auth = read_guest_json(&vm, &path).await.and_then(|v| v.as_object().cloned()).unwrap_or_default();
            auth.insert(pid.into(), json!({ "type": "api_key", "key": value }));
            let text = serde_json::to_string_pretty(&Value::Object(auth)).unwrap_or_default();
            vm.write_file(&path, format!("{text}\n").as_bytes()).await?;
            let _ = vm.sh(&format!("chmod 600 {path}"), &[]).await;
            k.research.log("user", Kind::HarnessLogin, json!({ "phase": "api_key", "harness": c.harness_id, "provider": pid }));
            finish_sign_in(k, pid, &format!("Pi in the app VM uses your {label} API key.")).await;
            Ok(())
        }
        other => Err(format!("unknown sign-in input \"{other}\"")),
    }
}

/// Sign out of one provider: remove its login from the VM's auth.json and restart Pi so it drops it. When it was the
/// default provider, another stored login becomes the default (none left: the sign-in card opens).
pub async fn logout(k: &Arc<Kernel>, provider: &str) -> Result<(), String> {
    let c = conn(k).ok_or("Pi is not running")?;
    let vm = k.vm().ok_or("the app VM is not running")?;
    let path = format!("{VM_PI_HOME}/auth.json");
    let mut auth = read_guest_json(&vm, &path).await.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let Some(gone) = auth.remove(provider) else {
        return Err(format!("there is no stored login for {}", provider_label(provider)));
    };
    let label = login_label(provider, gone.get("type").and_then(Value::as_str).unwrap_or(""));
    let text = serde_json::to_string_pretty(&Value::Object(auth.clone())).unwrap_or_default();
    vm.write_file(&path, format!("{text}\n").as_bytes()).await?;
    let _ = vm.sh(&format!("chmod 600 {path}"), &[]).await;
    let settings = read_guest_json(&vm, &format!("{VM_PI_HOME}/settings.json")).await;
    if settings.as_ref().and_then(|s| s.get("defaultProvider")).and_then(Value::as_str) == Some(provider) {
        let mut m = Map::new();
        m.insert("defaultProvider".into(), auth.keys().next().map(|p| json!(p)).unwrap_or(Value::Null));
        m.insert("defaultModel".into(), Value::Null);
        m.insert("defaultThinkingLevel".into(), Value::Null);
        if let Err(e) = merge_settings(&vm, m).await {
            log::warn!("pi settings: {e}");
        }
    }
    k.research.log("user", Kind::HarnessLogin, json!({ "phase": "signed_out", "harness": c.harness_id, "provider": provider }));
    k.system_message("system", &format!("Signed out: {label} is removed from the app VM."));
    harness::restart(k);
    Ok(())
}

/// Cancel an open sign-in; the card goes back to the choices.
pub fn login_cancel(k: &Arc<Kernel>) {
    let Some(c) = conn(k) else { return };
    if let Some(f) = c.login.locked().take() {
        let _ = f.tx.send(json!({ "cancel": true }).to_string());
        f.killer.kill();
    }
    k.update_harness(&c.harness_id, |o| {
        if o.signed_in != Some(true) {
            o.login = Some(required());
            o.note = Some("Sign in required: Pi in the app VM has its own login".into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_and_titles() {
        let p = system_prompt();
        assert!(p.starts_with(harness::CONTAINMENT) && p.contains(harness::BUILD_RULE) && p.contains(harness::VM_RULE) && p.contains("ask_user"));
        let m = turn_message("connect my Gmail", true);
        assert_eq!(m, "/skill:openapp-builder [OpenApp build mode] connect my Gmail", "the skill is inlined once, not also read");
        assert_eq!(turn_message("hi", false), "hi");
        assert_eq!(tool_title("mcp__openapp__openapp_describe", &Value::Null).as_deref(), Some("openapp.openapp_describe"));
        assert_eq!(tool_title("bash", &json!({ "command": "ls -la" })).as_deref(), Some("Run ls -la"));
        assert!(tool_title("ask_user", &Value::Null).is_none());
        assert_eq!(mcp_json()["mcpServers"]["openapp"]["headers"]["Authorization"], "Bearer ${OPENAPP_MCP_TOKEN}");
        assert_eq!(model_of(&json!({ "model": { "provider": "unknown", "id": "unknown" } })), None);
        assert_eq!(model_of(&json!({ "model": { "provider": "fakeco", "id": "fake-model-1" } })).as_deref(), Some("fakeco/fake-model-1"));
        assert!(EXTENSION_TS.contains(ASK_MARK));
    }

    #[test]
    fn refused_sign_in() {
        assert!(auth_refused("401 Unauthorized: token has expired"));
        assert!(auth_refused("Incorrect API key provided: sk-…"));
        assert!(auth_refused("Failed to refresh OAuth token for openai"));
        assert!(!auth_refused("Connection error."));
        assert!(!auth_refused("The 'fake-model' model is not supported when using Codex with a ChatGPT account."));
        assert_eq!(login_label("openai", "oauth"), "ChatGPT");
        assert_eq!(login_label("anthropic", "api_key"), "Anthropic API key");
    }

    #[test]
    fn failed_tool_text() {
        let r = json!({ "content": [{ "type": "text", "text": "ENOENT: no such file or directory, access '/workspace/x'" }] });
        assert!(failure_text(&r).starts_with("ENOENT"));
        assert_eq!(failure_text(&json!("fd failed")), "fd failed");
        assert_eq!(failure_text(&json!({ "error": { "message": "MCP error -32602" } })), "MCP error -32602");
        assert_eq!(failure_text(&Value::Null), "the tool failed without a message");
    }
}
