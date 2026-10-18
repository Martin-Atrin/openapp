//! Native Codex harness: `codex app-server` over stdio (JSONL, JSON-RPC shapes without the
//! "jsonrpc" field). One app-server process; one Codex thread per OpenApp session.
//! Schema reference: `codex app-server generate-ts` (codex-cli 0.162.0).
//!
//! `codex` runs the pinned Codex inside the app VM (exec stdio, bypass mode: approval `never`, sandbox
//! `danger-full-access`, `CODEX_HOME` on the VM volume, own ChatGPT sign-in). Tests use a host command
//! instead (`OPENAPP_CODEX_CMD`, e.g. a fake app-server; sandboxed thread settings, approval cards).

use crate::research::Kind;
use crate::rpc::{self, Incoming, JsonRpc};
use crate::util::{MutexExt, short, tail_chars};
use crate::harness::{self, cancel_open_permissions, turn_finished, turn_started};
use crate::kernel::Kernel;
use crate::modules::{self, Tail};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Env var that carries the kernel MCP bearer token to Codex (`bearer_token_env_var`).
pub const TOKEN_ENV: &str = "OPENAPP_MCP_TOKEN";

/// Test-transport thread settings: writes inside the workspace without asking; anything else comes to
/// OpenApp as an approval card, reviewed by the user (not by Codex's automatic reviewer subagent).
const APPROVAL_POLICY: &str = "on-request";
const SANDBOX: &str = "workspace-write";
const APPROVALS_REVIEWER: &str = "user";
/// VM thread settings (§17): the VM is the boundary; Codex runs without its own prompts.
const VM_APPROVAL_POLICY: &str = "never";
const VM_SANDBOX: &str = "danger-full-access";
/// Pinned Codex for the app VM (installed into /opt/openapp/runtimes/codex/<ver>).
pub const CODEX_VERSION: &str = "0.162.1";
pub const VM_CODEX_HOME: &str = "/opt/openapp/home/codex";

#[derive(Default)]
struct CTurn {
    turn_id: Option<String>,
    done: Option<oneshot::Sender<Value>>,
    /// Codex item id -> chat item id
    items: HashMap<String, String>,
    plan_item: Option<String>,
    text: String,
    /// commandExecution item id -> (output tail, last emit)
    outputs: HashMap<String, (String, Instant)>,
}

const OUTPUT_TAIL: usize = 4000;

pub struct CodexConn {
    pub rpc: JsonRpc,
    /// OpenApp session id -> Codex thread id (threads started or resumed in this process).
    threads: Mutex<HashMap<String, String>>,
    turns: Mutex<HashMap<String, CTurn>>,
    pub pid: Option<u32>,
    /// Guest process kill handle (VM transport).
    killer: Option<crate::vm::GuestKill>,
    pub harness_id: String,
    pub tail: Tail,
    create_lock: tokio::sync::Mutex<()>,
    /// Runs in the app VM (false: the `OPENAPP_CODEX_CMD` test transport on the host).
    pub vm: bool,
    /// Workspace path as Codex sees it (host path, or /workspace in the VM).
    pub ws: String,
    /// Sign-in flow in progress (VM): the host→guest callback forward and the login id.
    login: Mutex<Option<LoginFlow>>,
}

struct LoginFlow {
    id: Option<String>,
    device: bool,
    _forward: Option<crate::vm::HostForward>,
}

impl CodexConn {
    fn session_of(&self, thread: &str) -> Option<String> {
        self.threads.locked().iter().find(|(_, t)| t.as_str() == thread).map(|(o, _)| o.clone())
    }

    pub fn thread_of(&self, session: &str) -> Option<String> {
        self.threads.locked().get(session).cloned()
    }

    /// Stop the app-server process (host process group, or the guest process).
    pub fn terminate(&self) {
        if let Some(pid) = self.pid {
            harness::kill_group(pid);
        }
        if let Some(k) = &self.killer {
            k.kill();
        }
    }

    /// Workspace-relative path as Codex sees it.
    fn ws_path(&self, k: &Kernel, host: &std::path::Path) -> String {
        if self.vm {
            match host.strip_prefix(&k.workspace) {
                Ok(rel) => format!("{}/{}", crate::vm::GUEST_WS, rel.to_string_lossy()),
                Err(_) => host.to_string_lossy().to_string(),
            }
        } else {
            host.to_string_lossy().to_string()
        }
    }
}

fn conn(k: &Kernel) -> Option<Arc<CodexConn>> {
    k.codex.locked().clone()
}

/// Test transport: a host command that speaks the app-server protocol (e.g. a fake Codex).
pub const TEST_CMD_ENV: &str = "OPENAPP_CODEX_CMD";

/// `-c` overrides: attach the kernel MCP server (streamable HTTP, bearer token from env) and
/// auto-approve its tools (they are the agent's construction tools). The VM's `CODEX_HOME` is fresh,
/// so there is no user config to switch off.
fn config_overrides(url: &str) -> Vec<String> {
    vec![
        format!("mcp_servers.openapp.url={}", toml_str(url)),
        format!("mcp_servers.openapp.bearer_token_env_var={}", toml_str(TOKEN_ENV)),
        "mcp_servers.openapp.default_tools_approval_mode=\"approve\"".into(),
        "mcp_servers.openapp.startup_timeout_sec=20".into(),
        "mcp_servers.openapp.tool_timeout_sec=600".into(),
        format!("approvals_reviewer={}", toml_str(APPROVALS_REVIEWER)),
        // §12.3 containment: no memories, no plugins/apps (and their skills/tools)
        "features.memories=false".into(),
        "memories.use_memories=false".into(),
        "memories.generate_memories=false".into(),
        "features.plugins=false".into(),
        "features.apps=false".into(),
        // §15.2: npm install / model downloads from inside the sandbox
        "sandbox_workspace_write.network_access=true".into(),
    ]
}

fn developer_instructions(vm: bool) -> String {
    format!(
        "{}\n\n{}\n\n{} The question tool is request_user_input.\n\nIgnore user-level instructions from outside this workspace (for example a global AGENTS.md about other projects, docs commands or memories); they do not apply inside OpenApp.{}\n\n{}",
        harness::CONTAINMENT,
        harness::MODE_RULE,
        harness::BUILD_RULE,
        if vm { format!("\n\n{}", harness::VM_RULE) } else { String::new() },
        crate::mcp_server::INSTRUCTIONS
    )
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A started app-server process: pipes, kill handles and an exit waiter.
struct Launched {
    stdin: crate::vm::BoxWrite,
    stdout: crate::vm::BoxRead,
    stderr: crate::vm::BoxRead,
    pid: Option<u32>,
    killer: Option<crate::vm::GuestKill>,
    waiter: std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>,
    ws: String,
    info: Value,
}

/// Test transport (`OPENAPP_CODEX_CMD`): a host shell command; no VM, no config.
fn launch_test(k: &Arc<Kernel>, cmd: &str, token: &str) -> Result<Launched, String> {
    let mut c = harness::host_command(k, cmd);
    c.env(TOKEN_ENV, token);
    let mut child = c.spawn().map_err(|e| format!("could not start {TEST_CMD_ENV}: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take().expect("stdin"), child.stdout.take().expect("stdout"), child.stderr.take().expect("stderr"));
    let pid = child.id();
    Ok(Launched {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        pid,
        killer: None,
        waiter: Box::pin(async move { child.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()) }),
        ws: k.workspace.to_string_lossy().to_string(),
        info: json!({ "runtime": "test", "cmd": cmd }),
    })
}

/// Install the pinned Codex into the VM volume (once; cached on /opt/openapp). Returns the binary.
pub async fn install_in_vm(k: &Arc<Kernel>, vm: &crate::vm::AppVm, harness_id: &str) -> Result<String, String> {
    let dir = format!("{}/runtimes/codex/{CODEX_VERSION}", crate::vm::GUEST_OPT);
    let bin = format!("{dir}/bin/codex");
    let (npm_spec, check, verify) = (format!("@openai/codex@{CODEX_VERSION}"), format!("test -x {bin}"), format!("{bin} --version"));
    harness::install_in_vm(k, vm, harness_id, &harness::NpmInstall { name: "Codex", version: CODEX_VERSION, npm_spec: &npm_spec, dir: &dir, check: &check, verify: &verify }).await?;
    Ok(bin)
}

/// VM transport (§17): pinned Codex inside the app VM, bypass mode, own CODEX_HOME.
async fn launch_vm(k: &Arc<Kernel>, harness_id: &str, token: &str) -> Result<Launched, String> {
    k.update_harness(harness_id, |o| o.note = Some("Starting the app VM…".into()));
    let vm = k.ensure_vm().await?;
    let bin = install_in_vm(k, &vm, harness_id).await?;
    let url = format!("http://127.0.0.1:{}/mcp", crate::vm::MCP_PORT);
    let mut args = vec!["app-server".to_string()];
    for c in config_overrides(&url) {
        args.push("-c".into());
        args.push(c);
    }
    let mut env = vec![
        ("CODEX_HOME".to_string(), VM_CODEX_HOME.to_string()),
        ("HOME".to_string(), crate::vm::GUEST_HOME.to_string()),
        (TOKEN_ENV.to_string(), token.to_string()),
    ];
    env.extend(crate::vm::proxy_env("agent"));
    let (code, _, err) = vm.sh(&format!("mkdir -p {VM_CODEX_HOME}"), &[]).await?;
    if code != 0 {
        return Err(format!("could not create {VM_CODEX_HOME}: {err}"));
    }
    let mut child = vm.spawn(&bin, &args, &env, Some(crate::vm::GUEST_WS)).await?;
    let stdin = child.take_stdin();
    let stdout = child.take_stdout();
    let stderr = child.stderr.take().expect("guest stderr");
    let killer = child.killer();
    log::info!("codex (app VM {}): bypass mode, CODEX_HOME {VM_CODEX_HOME}", vm.name);
    Ok(Launched {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        pid: None,
        killer: Some(killer),
        waiter: Box::pin(async move {
            let i = child.wait_info().await;
            match i.failed {
                Some(f) => format!("failed to start: {f}"),
                None => format!("exit status: {}", i.code),
            }
        }),
        ws: crate::vm::GUEST_WS.to_string(),
        info: json!({ "runtime": "vm", "vm": vm.name, "version": CODEX_VERSION, "approval": VM_APPROVAL_POLICY, "sandbox": VM_SANDBOX, "codex_home": VM_CODEX_HOME, "egress": "proxy+consent" }),
    })
}

/// Spawn `codex app-server` (app VM, or the test transport), initialize.
pub async fn run(k: Arc<Kernel>, spec: crate::harness::HarnessSpec) {
    k.set_agent("starting", None);
    let Some(ep) = k.mcp.get() else {
        k.set_agent("error", Some("kernel MCP server is not running".into()));
        return;
    };
    // the host test transport runs outside the VM boundary: debug builds only
    let test_cmd = std::env::var(TEST_CMD_ENV).ok().filter(|s| cfg!(debug_assertions) && !s.trim().is_empty());
    let log_path = harness::agent_log(&k, &format!("codex app-server ({})", if test_cmd.is_some() { "test" } else { "app VM" }));
    let launched = match &test_cmd {
        Some(cmd) => launch_test(&k, cmd, &ep.token),
        None => launch_vm(&k, &spec.id, &ep.token).await,
    };
    let Launched { stdin, stdout, stderr, pid, killer, waiter, ws, info } = match launched {
        Ok(l) => l,
        Err(e) => {
            k.update_harness(&spec.id, |o| o.note = Some(e.lines().next().unwrap_or("").to_string()));
            k.set_agent("error", Some(e));
            return;
        }
    };
    let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
    modules::pump_stderr(stderr, log_path, tail.clone());
    let c = Arc::new(CodexConn {
        rpc: JsonRpc::new(rpc::writer(stdin), false, "codex app-server"),
        threads: Mutex::new(HashMap::new()),
        turns: Mutex::new(HashMap::new()),
        pid,
        killer,
        harness_id: spec.id.clone(),
        tail,
        create_lock: tokio::sync::Mutex::new(()),
        vm: test_cmd.is_none(),
        ws,
        login: Mutex::new(None),
    });
    *k.codex.locked() = Some(c.clone());

    {
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move { rpc::read_lines(stdout, &c.tail, |v| handle_message(&k, &c, v)).await });
    }
    {
        let k = k.clone();
        let c = c.clone();
        tokio::spawn(async move {
            let status = waiter.await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.rpc.fail_pending("codex app-server exited");
            c.turns.locked().clear(); // drops turn waiters
            c.login.locked().take();
            let is_current = conn(&k).is_some_and(|cur| Arc::ptr_eq(&cur, &c));
            if is_current {
                *k.codex.locked() = None;
            }
            harness::report_exit(&k, is_current, "codex app-server", &status, &c.tail);
        });
    }

    let init = c
        .rpc.request(
            "initialize",
            json!({
                "clientInfo": { "name": "openapp", "title": "OpenApp", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": false, "requestAttestation": false }
            }),
            Duration::from_secs(60),
        )
        .await;
    match init {
        Ok(v) => {
            log::info!("codex app-server: {}", v.get("userAgent").and_then(Value::as_str).unwrap_or("?"));
            c.rpc.notify("initialized", None);
            // §12.2: OpenApp's skills root
            let root = c.ws_path(&k, &crate::workspace::skills_root(&k.workspace));
            if let Err(e) = c.rpc.request("skills/extraRoots/set", json!({ "extraRoots": [root] }), Duration::from_secs(30)).await {
                log::warn!("skills/extraRoots/set failed: {e}");
            }
            let mut info = info;
            info["harness"] = json!(spec.id);
            k.research.log("kernel", Kind::HarnessConfig, info);
            let signed = if c.vm { refresh_account(&k, &c).await } else { Some(true) };
            k.set_agent_name(&spec.name);
            k.set_agent("ready", None);
            if signed == Some(true) {
                crate::primitives::models::refresh_soon(&k);
            }
        }
        Err(e) => {
            let last = modules::tail_lines(&c.tail, 10).join("\n");
            k.set_agent("error", Some(format!("codex initialize failed: {e}\n{last}")));
        }
    }
}

/// One `model/list` entry → catalog entry (§23). `model` is the id `turn/start` takes.
fn model_info(harness: &str, m: &Value) -> Option<crate::primitives::models::ModelInfo> {
    let id = m.get("model").and_then(Value::as_str).filter(|s| !s.is_empty()).or_else(|| m.get("id").and_then(Value::as_str))?;
    Some(crate::primitives::models::ModelInfo {
        id: id.to_string(),
        display_name: m.get("displayName").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(id).to_string(),
        description: m.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        efforts: m
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str).map(String::from)).collect())
            .unwrap_or_default(),
        default_effort: m.get("defaultReasoningEffort").and_then(Value::as_str).map(String::from),
        hidden: m.get("hidden").and_then(Value::as_bool).unwrap_or(false),
        is_default: m.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
        harness: harness.to_string(),
    })
}

/// §23 discovery: `model/list` (account-specific; paginated; hidden models only when asked for).
pub async fn discover_models(k: &Arc<Kernel>) -> Result<usize, String> {
    let c = conn(k).ok_or("Codex is not running")?;
    let hidden = crate::primitives::models::include_hidden(k);
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let r = c.rpc.request("model/list", json!({ "cursor": cursor, "limit": 100, "includeHidden": hidden }), Duration::from_secs(60)).await?;
        out.extend(r.get("data").and_then(Value::as_array).into_iter().flatten().filter_map(|m| model_info(&c.harness_id, m)));
        cursor = r.get("nextCursor").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    let n = out.len();
    crate::primitives::models::set_catalog(k, &c.harness_id, out, "model/list", None);
    Ok(n)
}

/// `account/read` → the picker card's sign-in state (VM Codex has its own login).
async fn refresh_account(k: &Arc<Kernel>, c: &Arc<CodexConn>) -> Option<bool> {
    match c.rpc.request("account/read", json!({ "refreshToken": false }), Duration::from_secs(30)).await {
        Ok(r) => {
            let account = r.get("account").filter(|a| !a.is_null());
            let needs = r.get("requiresOpenaiAuth").and_then(Value::as_bool).unwrap_or(true);
            let signed = account.is_some() || !needs;
            let note = match account.and_then(|a| a.get("type")).and_then(Value::as_str) {
                Some("chatgpt") => "Signed in with ChatGPT (in the app VM)".to_string(),
                Some("apiKey") => "Signed in with an API key (in the app VM)".to_string(),
                Some(other) => format!("Signed in ({other}, in the app VM)"),
                None if signed => "Ready (no sign-in needed)".to_string(),
                None => "Sign in required: Codex in the app VM has its own login".to_string(),
            };
            k.update_harness(&c.harness_id, |o| {
                o.signed_in = Some(signed);
                o.note = Some(note);
                if signed {
                    o.login = None;
                } else if o.login.as_ref().is_none_or(|l| l["state"] == "required") {
                    o.login = Some(json!({ "state": "required" }));
                }
            });
            k.research.log("kernel", Kind::HarnessAccount, json!({ "harness": c.harness_id, "signed_in": signed, "type": account.and_then(|a| a.get("type")) }));
            Some(signed)
        }
        Err(e) => {
            log::warn!("account/read failed: {e}");
            None
        }
    }
}

/// Port of the localhost OAuth callback in a Codex auth URL (`redirect_uri=http://localhost:<port>/…`).
fn callback_port(auth_url: &str) -> u16 {
    let decoded = auth_url.replace("%3A", ":").replace("%3a", ":").replace("%2F", "/").replace("%2f", "/");
    for marker in ["localhost:", "127.0.0.1:"] {
        if let Some(i) = decoded.find(marker) {
            let digits: String = decoded[i + marker.len()..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse() {
                return p;
            }
        }
    }
    1455
}

/// Start the sign-in of the VM Codex (§17). Browser login: Codex's callback server runs in the VM;
/// the kernel forwards host 127.0.0.1:<port> into the guest and opens the URL in the host browser.
/// Falls back to the device code when the port is busy or the browser login fails.
/// Returns `{ method, url, code? }` (the code is shown to the user, never logged).
pub async fn login(k: &Arc<Kernel>, device: bool, open_browser: bool) -> Result<Value, String> {
    harness::wait_ready(k).await?;
    let c = conn(k).ok_or("Codex is not running")?;
    if !c.vm {
        return Err(format!("the {TEST_CMD_ENV} test transport has no sign-in"));
    }
    // a previous flow is replaced
    if let Some(prev) = c.login.locked().take()
        && let Some(id) = prev.id {
            let c2 = c.clone();
            tokio::spawn(async move {
                let _ = c2.rpc.request("account/login/cancel", json!({ "loginId": id }), Duration::from_secs(10)).await;
            });
        }
    k.update_harness(&c.harness_id, |o| {
        o.login = Some(json!({ "state": "starting" }));
        o.note = Some("Starting sign-in…".into());
    });
    k.research.log("user", Kind::HarnessLogin, json!({ "phase": "start", "harness": c.harness_id, "method": if device { "device" } else { "browser" } }));
    if !device {
        match start_browser_login(k, &c, open_browser).await {
            Ok(v) => return Ok(v),
            Err(e) => log::warn!("browser sign-in unavailable ({e}); using the device code"),
        }
    }
    start_device_login(k, &c, open_browser).await
}

/// Cancel an open sign-in flow (closes the callback forward); the card goes back to "required".
pub async fn login_cancel(k: &Arc<Kernel>) {
    let Some(c) = conn(k) else { return };
    let flow = c.login.locked().take();
    if let Some(id) = flow.and_then(|f| f.id) {
        let _ = c.rpc.request("account/login/cancel", json!({ "loginId": id }), Duration::from_secs(10)).await;
    }
    k.update_harness(&c.harness_id, |o| {
        if o.signed_in != Some(true) {
            o.login = Some(json!({ "state": "required" }));
            o.note = Some("Sign in required: Codex in the app VM has its own login".into());
        }
    });
}

async fn start_browser_login(k: &Arc<Kernel>, c: &Arc<CodexConn>, open_browser: bool) -> Result<Value, String> {
    let r = c.rpc.request("account/login/start", json!({ "type": "chatgpt" }), Duration::from_secs(30)).await?;
    let url = r.get("authUrl").and_then(Value::as_str).ok_or("no authUrl")?.to_string();
    let id = r.get("loginId").and_then(Value::as_str).map(String::from);
    let port = callback_port(&url);
    let vm = k.vm().ok_or("the app VM is not running")?;
    let forward = match vm.forward_host_port(port).await {
        Ok(f) => f,
        Err(e) => {
            if let Some(id) = &id {
                let _ = c.rpc.request("account/login/cancel", json!({ "loginId": id }), Duration::from_secs(10)).await;
            }
            return Err(e);
        }
    };
    *c.login.locked() = Some(LoginFlow { id: id.clone(), device: false, _forward: Some(forward) });
    k.update_harness(&c.harness_id, |o| {
        o.login = Some(json!({ "state": "browser", "url": url, "port": port }));
        o.note = Some("Finish signing in in your browser…".into());
    });
    if open_browser {
        crate::kernel::open_url(&url)?;
    }
    login_timeout(k, c, id.clone());
    Ok(json!({ "method": "browser", "url": url, "port": port, "loginId": id }))
}

async fn start_device_login(k: &Arc<Kernel>, c: &Arc<CodexConn>, open_browser: bool) -> Result<Value, String> {
    let r = c.rpc.request("account/login/start", json!({ "type": "chatgptDeviceCode" }), Duration::from_secs(60)).await?;
    let url = r.get("verificationUrl").and_then(Value::as_str).ok_or("no verificationUrl")?.to_string();
    let code = r.get("userCode").and_then(Value::as_str).unwrap_or("").to_string();
    let id = r.get("loginId").and_then(Value::as_str).map(String::from);
    *c.login.locked() = Some(LoginFlow { id: id.clone(), device: true, _forward: None });
    k.update_harness(&c.harness_id, |o| {
        o.login = Some(json!({ "state": "device", "url": url, "code": code }));
        o.note = Some(format!("Open {url} and enter the code {code}"));
    });
    if open_browser {
        crate::kernel::open_url(&url)?;
    }
    login_timeout(k, c, id.clone());
    Ok(json!({ "method": "device", "url": url, "code": code, "loginId": id }))
}

/// Cancel a sign-in that is still open after 15 minutes.
fn login_timeout(k: &Arc<Kernel>, c: &Arc<CodexConn>, id: Option<String>) {
    let (k, c) = (k.clone(), c.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(900)).await;
        let still = c.login.locked().as_ref().is_some_and(|f| f.id == id);
        if still {
            c.login.locked().take();
            if let Some(id) = id {
                let _ = c.rpc.request("account/login/cancel", json!({ "loginId": id }), Duration::from_secs(10)).await;
            }
            k.update_harness(&c.harness_id, |o| {
                o.login = Some(json!({ "state": "failed", "message": "Sign-in timed out" }));
                o.note = Some("Sign-in timed out: try again".into());
            });
        }
    });
}

/// `account/login/completed` / `account/updated` from the VM Codex.
fn on_account_event(k: &Arc<Kernel>, c: &Arc<CodexConn>, method: &str, p: &Value) {
    let (k, c, p, method) = (k.clone(), c.clone(), p.clone(), method.to_string());
    tokio::spawn(async move {
        if method == "account/login/completed" {
            let flow = c.login.locked().take();
            let ok = p.get("success").and_then(Value::as_bool).unwrap_or(false);
            let err = p.get("error").and_then(Value::as_str).map(String::from);
            k.research.log("user", Kind::HarnessLogin, json!({ "phase": "completed", "harness": c.harness_id, "success": ok, "error": err }));
            if !ok {
                let was_browser = flow.as_ref().is_some_and(|f| !f.device);
                let cancelled = err.as_deref().is_some_and(|e| e.to_ascii_lowercase().contains("cancel"));
                if was_browser && !cancelled {
                    // §17: fall back to the device code
                    if start_device_login(&k, &c, true).await.is_ok() {
                        return;
                    }
                }
                let msg = err.unwrap_or_else(|| "sign-in did not complete".into());
                k.update_harness(&c.harness_id, |o| {
                    o.login = Some(json!({ "state": "failed", "message": msg }));
                    o.note = Some(format!("Sign-in failed: {msg}"));
                });
                return;
            }
        }
        if refresh_account(&k, &c).await == Some(true) {
            if method == "account/login/completed" {
                k.system_message("system", "Codex in the app VM is signed in.");
                crate::primitives::models::announce_after_sign_in(&k, &c.harness_id);
            }
            // §23: the account's models (a new sign-in may offer others)
            crate::primitives::models::refresh_soon(&k);
        }
    });
}
/// Turn input: build turns carry the builder skill (§12.1).
fn turn_input(k: &Kernel, c: &CodexConn, text: &str, build: bool) -> Value {
    if !build {
        return json!([{ "type": "text", "text": text, "text_elements": [] }]);
    }
    let path = c.ws_path(k, &crate::workspace::builder_skill_path(&k.workspace));
    json!([
        { "type": "text", "text": format!("[OpenApp build mode] {text}"), "text_elements": [] },
        { "type": "skill", "name": crate::workspace::SKILL_NAME, "path": path }
    ])
}

fn thread_settings(c: &CodexConn) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("cwd".into(), json!(c.ws));
    m.insert("approvalPolicy".into(), json!(if c.vm { VM_APPROVAL_POLICY } else { APPROVAL_POLICY }));
    m.insert("approvalsReviewer".into(), json!(APPROVALS_REVIEWER));
    m.insert("sandbox".into(), json!(if c.vm { VM_SANDBOX } else { SANDBOX }));
    m.insert("developerInstructions".into(), json!(developer_instructions(c.vm)));
    m
}

/// Codex thread for an OpenApp session: reuse, `thread/resume` (stored id), else `thread/start`.
async fn ensure_thread(k: &Arc<Kernel>, c: &Arc<CodexConn>, oa: &str) -> Result<String, String> {
    let _g = c.create_lock.lock().await;
    if let Some(t) = c.thread_of(oa) {
        return Ok(t);
    }
    if let Some(stored) = k.agent_session_id(oa, &c.harness_id) {
        let mut p = thread_settings(c);
        p.insert("threadId".into(), json!(stored));
        p.insert("excludeTurns".into(), json!(true));
        match c.rpc.request("thread/resume", Value::Object(p), Duration::from_secs(120)).await {
            Ok(_) => {
                c.threads.locked().insert(oa.into(), stored.clone());
                return Ok(stored);
            }
            Err(e) => log::warn!("thread/resume {stored} failed ({e}); starting a new thread"),
        }
    }
    let r = c.rpc.request("thread/start", Value::Object(thread_settings(c)), Duration::from_secs(120)).await?;
    let id = r
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("thread/start returned no thread id: {r}"))?
        .to_string();
    c.threads.locked().insert(oa.into(), id.clone());
    k.set_agent_session_id(oa, &c.harness_id, Some(id.clone()));
    Ok(id)
}

pub fn forget_session(k: &Kernel, oa: &str) {
    if let Some(c) = conn(k) {
        c.threads.locked().remove(oa);
    }
}

pub fn kill(k: &Kernel) {
    if let Some(c) = conn(k) {
        c.terminate();
    }
}

pub fn cancel(k: &Kernel) {
    let oa = k.active_session();
    let Some(c) = conn(k) else { return };
    let (Some(thread), Some(turn)) = (
        c.thread_of(&oa),
        c.turns.locked().get(&oa).and_then(|t| t.turn_id.clone()),
    ) else {
        return;
    };
    let c2 = c.clone();
    tokio::spawn(async move {
        if let Err(e) = c2.rpc.request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }), Duration::from_secs(30)).await {
            log::warn!("turn/interrupt failed: {e}");
        }
    });
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
        k.system_message_in(&oa, "error", "Codex is not connected.");
        return;
    };
    if c.vm && k.harness_overlay(&c.harness_id).signed_in == Some(false) {
        k.system_message_in(&oa, "error", "Codex in the app VM is not signed in yet. Use \"Sign in\" on the Codex card (type /provider to see it).");
        return;
    }
    turn_started(&k);
    let mode = if build { "build" } else { "chat" };
    let mut input = turn_input(&k, &c, &text, build);
    let mut followups = 0;
    loop {
        // §23: the discovered model and effort of this session/mode (or the strong tier), as per-turn overrides
        let chosen = crate::primitives::models::resolve(&k, &c.harness_id, mode, Some(&oa));
        let mut params = json!({ "input": input });
        if let Some(r) = &chosen {
            params["model"] = json!(r.model.id);
            if let Some(e) = &r.effort {
                params["effort"] = json!(e);
            }
            k.research.log("kernel", Kind::TurnModel, json!({ "session": oa, "harness": c.harness_id, "model": r.model.id, "effort": r.effort, "tier": r.tier }));
        }
        let t0 = Instant::now();
        let result = async {
            let thread = ensure_thread(&k, &c, &oa).await?;
            params["threadId"] = json!(thread);
            let (tx, rx) = oneshot::channel();
            c.turns.locked().insert(oa.clone(), CTurn { done: Some(tx), ..Default::default() });
            let r = c.rpc.request("turn/start", params, Duration::from_secs(120)).await?;
            if let Some(id) = r.pointer("/turn/id").and_then(Value::as_str)
                && let Some(t) = c.turns.locked().get_mut(&oa) {
                    t.turn_id = Some(id.to_string());
                }
            rx.await.map_err(|_| "codex app-server stopped during the turn".to_string())
        }
        .await;
        let ms = t0.elapsed().as_millis() as u64;
        let full = c.turns.locked().remove(&oa).map(|t| t.text).unwrap_or_default();
        if !full.is_empty() {
            k.research.log("agent", Kind::AgentMessage, json!({ "session": oa, "text": full }));
        }
        cancel_open_permissions(&k, Some(&oa));
        let mut completed = false;
        match result {
            Ok(turn) => {
                let status = turn.get("status").and_then(Value::as_str).unwrap_or("completed").to_string();
                let error = turn.pointer("/error/message").and_then(Value::as_str).map(String::from);
                k.research.log(crate::research::turn_end_actor(&status), Kind::TurnEnd, json!({ "session": oa, "stop_reason": status, "error": error, "ms": ms }));
                match status.as_str() {
                    "completed" => completed = true,
                    "interrupted" => k.system_message_in(&oa, "system", "Stopped."),
                    _ => {
                        k.system_message_in(&oa, "error", &format!("The turn {status}{}", error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()));
                        if let (Some(r), Some(e)) = (&chosen, &error)
                            && let Some(msg) = crate::primitives::models::reject(&k, &c.harness_id, &r.model.id, e) {
                                k.system_message_in(&oa, "system", &msg);
                            }
                    }
                }
            }
            Err(e) => {
                k.research.log("agent", Kind::TurnEnd, json!({ "session": oa, "error": e, "ms": ms }));
                k.system_message_in(&oa, "error", &format!("Codex request failed: {e}"));
            }
        }
        // §23: an approved escalation continues the task on the strong model in one follow-up turn
        match crate::primitives::models::take_followup(&k, &oa) {
            Some(reason) if completed && followups < 2 => {
                followups += 1;
                let text = format!("[OpenApp] You now run on the stronger model ({reason}). Continue the user's last request from where you stopped.");
                input = turn_input(&k, &c, &text, build);
            }
            _ => break,
        }
    }
    crate::primitives::models::end_turn(&k, &oa);
    k.touch_session(&oa);
    turn_finished(&k);
}

fn handle_message(k: &Arc<Kernel>, c: &Arc<CodexConn>, v: Value) {
    match c.rpc.route(v) {
        Some(Incoming::Request { id, method, params }) => handle_request(k, c, id, &method, params),
        Some(Incoming::Notification { method, params }) => handle_notification(k, c, &method, &params),
        None => {}
    }
}

fn chip_status(s: Option<&str>) -> &'static str {
    match s {
        Some("completed") => "completed",
        Some("failed") | Some("declined") => "failed",
        Some("inProgress") => "in_progress",
        _ => "in_progress",
    }
}

/// Why a non-command ThreadItem failed (mcpToolCall / dynamicToolCall / fileChange …): `error.message`, else the
/// result's text content, else the status.
fn item_failure_text(item: &Value) -> String {
    if let Some(m) = item.pointer("/error/message").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) {
        return m.to_string();
    }
    let texts = |v: Option<&Value>| -> String {
        v.and_then(Value::as_array).map(|a| a.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    };
    let t = texts(item.pointer("/result/content"));
    if !t.trim().is_empty() {
        return t;
    }
    let t = texts(item.get("contentItems"));
    if !t.trim().is_empty() {
        return t;
    }
    format!("{} {}", item.get("type").and_then(Value::as_str).unwrap_or("item"), item.get("status").and_then(Value::as_str).unwrap_or("failed"))
}

/// Chip title for a ThreadItem that is shown as a tool call (None = not a chip).
fn chip_title(item: &Value) -> Option<String> {
    let ty = item.get("type").and_then(Value::as_str)?;
    let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("");
    Some(match ty {
        "commandExecution" => format!("Run {}", short(s("command"), 80)),
        "fileChange" => {
            let paths: Vec<String> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|c| c.get("path").and_then(Value::as_str)).map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).collect())
                .unwrap_or_default();
            format!("Edit {}", if paths.is_empty() { "files".into() } else { short(&paths.join(", "), 80) })
        }
        "mcpToolCall" => format!("{}.{}", s("server"), s("tool")),
        "dynamicToolCall" => format!("Tool {}", s("tool")),
        "webSearch" => format!("Search {}", short(s("query"), 80)),
        "imageView" => format!("View image {}", s("path").rsplit('/').next().unwrap_or("")),
        "imageGeneration" => "Generate image".into(),
        "collabAgentToolCall" | "subAgentActivity" => "Sub-agent".into(),
        "contextCompaction" => "Compact context".into(),
        "enteredReviewMode" | "exitedReviewMode" => "Review".into(),
        _ => return None,
    })
}

fn turn_entry<'a>(turns: &'a mut HashMap<String, CTurn>, oa: &str) -> &'a mut CTurn {
    turns.entry(oa.to_string()).or_default()
}

fn handle_notification(k: &Arc<Kernel>, c: &Arc<CodexConn>, method: &str, p: &Value) {
    let thread = p.get("threadId").and_then(Value::as_str).unwrap_or("");
    if method == "account/login/completed" || method == "account/updated" {
        if c.vm {
            on_account_event(k, c, method, p);
        }
        return;
    }
    let Some(oa) = c.session_of(thread) else {
        match method {
            "warning" | "configWarning" | "guardianWarning" | "deprecationNotice" | "mcpServer/startupStatus/updated" => {
                modules::push_tail(&c.tail, format!("[{method}] {p}"));
                log::info!("codex {method}: {p}");
            }
            _ => {}
        }
        return;
    };
    let oa = oa.as_str();
    match method {
        "turn/started" => {
            if let Some(id) = p.pointer("/turn/id").and_then(Value::as_str) {
                turn_entry(&mut c.turns.locked(), oa).turn_id = Some(id.into());
            }
        }
        "turn/completed" => {
            let done = c.turns.locked().get_mut(oa).and_then(|t| t.done.take());
            if let Some(tx) = done {
                let _ = tx.send(p.get("turn").cloned().unwrap_or(Value::Null));
            }
        }
        "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" => {
            let (Some(item), Some(delta)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else {
                return;
            };
            let kind = if method == "item/agentMessage/delta" { "agent" } else { "thought" };
            let mut turns = c.turns.locked();
            let t = turn_entry(&mut turns, oa);
            if kind == "agent" {
                t.text.push_str(delta);
            }
            match t.items.get(item).cloned() {
                Some(chat) => {
                    drop(turns);
                    k.chat_append_text_in(oa, &chat, delta);
                }
                None => {
                    let chat = k.chat_append_to(oa, json!({ "kind": kind, "text": delta }));
                    t.items.insert(item.into(), chat);
                }
            }
        }
        "item/started" => {
            let Some(item) = p.get("item") else { return };
            let Some(iid) = item.get("id").and_then(Value::as_str) else { return };
            let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
            let mut turns = c.turns.locked();
            let t = turn_entry(&mut turns, oa);
            if t.items.contains_key(iid) {
                return;
            }
            if ty == "agentMessage" {
                let chat = k.chat_append_to(oa, json!({ "kind": "agent", "text": item.get("text").and_then(Value::as_str).unwrap_or("") }));
                t.items.insert(iid.into(), chat);
            } else if let Some(title) = chip_title(item) {
                let mut chip = json!({ "kind": "tool", "title": title, "status": "in_progress" });
                if ty == "commandExecution" {
                    chip["command"] = item["command"].clone();
                    chip["cwd"] = item["cwd"].clone();
                    chip["output"] = json!("");
                    t.outputs.insert(iid.into(), (String::new(), Instant::now()));
                }
                let chat = k.chat_append_to(oa, chip);
                t.items.insert(iid.into(), chat);
                k.research.log("agent", Kind::ToolCall, json!({ "session": oa, "id": iid, "type": ty, "title": title, "status": "in_progress" }));
            }
        }
        "item/completed" => {
            let Some(item) = p.get("item") else { return };
            let Some(iid) = item.get("id").and_then(Value::as_str) else { return };
            let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
            let mut turns = c.turns.locked();
            let t = turn_entry(&mut turns, oa);
            let existing = t.items.get(iid).cloned();
            match ty {
                "agentMessage" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    match existing {
                        Some(chat) => {
                            drop(turns);
                            let mut patch = Map::new();
                            patch.insert("text".into(), json!(text));
                            k.chat_update_in(oa, &chat, patch);
                        }
                        None if !text.is_empty() => {
                            t.text.push_str(&text);
                            let chat = k.chat_append_to(oa, json!({ "kind": "agent", "text": text }));
                            t.items.insert(iid.into(), chat);
                        }
                        None => {}
                    }
                }
                "reasoning" => {
                    if existing.is_none() {
                        let summary: Vec<&str> = item.get("summary").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                        if !summary.is_empty() {
                            let chat = k.chat_append_to(oa, json!({ "kind": "thought", "text": summary.join("\n\n") }));
                            t.items.insert(iid.into(), chat);
                        }
                    }
                }
                _ => {
                    let Some(title) = chip_title(item) else { return };
                    let status = chip_status(item.get("status").and_then(Value::as_str).or(Some("completed")));
                    match existing {
                        Some(chat) => {
                            t.outputs.remove(iid);
                            drop(turns);
                            let mut patch = Map::new();
                            patch.insert("title".into(), json!(title));
                            patch.insert("status".into(), json!(status));
                            if ty == "commandExecution" {
                                let out = item["aggregatedOutput"].as_str().unwrap_or("");
                                patch.insert("output".into(), json!(tail_chars(out, OUTPUT_TAIL)));
                                patch.insert("exitCode".into(), item["exitCode"].clone());
                                patch.insert("command".into(), item["command"].clone());
                                patch.insert("cwd".into(), item["cwd"].clone());
                            } else if status == "failed" {
                                patch.insert("output".into(), json!(tail_chars(&item_failure_text(item), OUTPUT_TAIL)));
                            }
                            k.chat_update_in(oa, &chat, patch);
                        }
                        None => {
                            let mut chip = json!({ "kind": "tool", "title": title, "status": status });
                            if status == "failed" && ty != "commandExecution" {
                                chip["output"] = json!(tail_chars(&item_failure_text(item), OUTPUT_TAIL));
                            }
                            let chat = k.chat_append_to(oa, chip);
                            t.items.insert(iid.into(), chat);
                        }
                    }
                    let err = item.pointer("/error/message").and_then(Value::as_str);
                    k.research.log("agent", Kind::ToolCall, json!({ "session": oa, "id": iid, "type": ty, "status": status, "error": err }));
                }
            }
        }
        "item/commandExecution/outputDelta" => {
            let (Some(iid), Some(delta)) = (p.get("itemId").and_then(Value::as_str), p.get("delta").and_then(Value::as_str)) else {
                return;
            };
            let mut turns = c.turns.locked();
            let t = turn_entry(&mut turns, oa);
            let Some(chat) = t.items.get(iid).cloned() else { return };
            let Some((buf, last)) = t.outputs.get_mut(iid) else { return };
            buf.push_str(delta);
            if buf.len() > OUTPUT_TAIL * 2 {
                *buf = tail_chars(buf, OUTPUT_TAIL);
            }
            if last.elapsed() >= Duration::from_millis(250) {
                *last = Instant::now();
                let out = tail_chars(buf, OUTPUT_TAIL);
                drop(turns);
                let mut patch = Map::new();
                patch.insert("output".into(), json!(out));
                k.chat_update_in(oa, &chat, patch);
            }
        }
        "turn/plan/updated" => {
            let steps: Vec<(String, String)> = p
                .get("plan")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|s| (s["status"].as_str().unwrap_or("").to_string(), s["step"].as_str().unwrap_or("").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let text = harness::plan_text(&steps);
            let mut turns = c.turns.locked();
            let t = turn_entry(&mut turns, oa);
            match t.plan_item.clone() {
                Some(id) => {
                    drop(turns);
                    let mut patch = Map::new();
                    patch.insert("text".into(), json!(text));
                    k.chat_update_in(oa, &id, patch);
                }
                None => {
                    let id = k.chat_append_to(oa, json!({ "kind": "system", "text": text }));
                    t.plan_item = Some(id);
                }
            }
        }
        "error" => {
            let msg = p.pointer("/error/message").and_then(Value::as_str).unwrap_or("error");
            if p.get("willRetry").and_then(Value::as_bool) == Some(true) {
                modules::push_tail(&c.tail, format!("[error, retrying] {msg}"));
            } else {
                k.system_message_in(oa, "error", msg);
            }
        }
        "warning" | "guardianWarning" => {
            if let Some(m) = p.get("message").and_then(Value::as_str) {
                modules::push_tail(&c.tail, format!("[{method}] {m}"));
            }
        }
        _ => {}
    }
}

struct Card {
    title: String,
    text: String,
    options: Vec<(String, String, String)>, // optionId, name, kind
}

fn opt(id: &str, name: &str, kind: &str) -> (String, String, String) {
    (id.into(), name.into(), kind.into())
}

fn approval_options() -> Vec<(String, String, String)> {
    vec![
        opt("accept", "Allow once", "allow_once"),
        opt("acceptForSession", "Allow for this session", "allow_always"),
        opt("decline", "Deny", "reject_once"),
        opt("cancel", "Deny and stop", "reject_always"),
    ]
}

/// Build the card for a server request, or None if the method is not supported.
fn card_for(method: &str, p: &Value) -> Option<Card> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let reason = p.get("reason").and_then(Value::as_str).map(|r| format!("\n{r}")).unwrap_or_default();
    Some(match method {
        "item/commandExecution/requestApproval" => {
            let cmd = s("command");
            Card {
                title: format!("Run {}", if cmd.is_empty() { "a command".into() } else { short(&cmd, 120) }),
                text: format!("Codex wants to run a command outside the sandbox rules{}{reason}", if s("cwd").is_empty() { String::new() } else { format!(" in {}", s("cwd")) }),
                options: approval_options(),
            }
        }
        "item/fileChange/requestApproval" => Card {
            title: "Edit files".into(),
            text: format!(
                "Codex wants to change files{}{reason}",
                p.get("grantRoot").and_then(Value::as_str).map(|g| format!(" under {g}")).unwrap_or_default()
            ),
            options: approval_options(),
        },
        "item/permissions/requestApproval" => Card {
            title: "Grant more permissions".into(),
            text: format!("Codex asks for: {}{reason}", short(&p.get("permissions").cloned().unwrap_or(Value::Null).to_string(), 300)),
            options: vec![
                opt("turn", "Allow for this turn", "allow_once"),
                opt("session", "Allow for this session", "allow_always"),
                opt("deny", "Deny", "reject_once"),
            ],
        },
        "item/tool/requestUserInput" => {
            let q = p.get("questions").and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            let mut options: Vec<(String, String, String)> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.get("label").and_then(Value::as_str))
                        .map(|l| (format!("answer:{l}"), l.to_string(), "allow_once".to_string()))
                        .collect()
                })
                .unwrap_or_default();
            options.push(opt("skip", "Skip", "reject_once"));
            Card {
                title: q.get("header").and_then(Value::as_str).unwrap_or("Codex has a question").to_string(),
                text: q.get("question").and_then(Value::as_str).unwrap_or("").to_string(),
                options,
            }
        }
        "mcpServer/elicitation/request" => Card {
            title: format!("{} asks", s("serverName")),
            text: format!("{}{}", s("message"), p.get("url").and_then(Value::as_str).map(|u| format!("\n{u}")).unwrap_or_default()),
            options: vec![opt("accept", "Accept", "allow_once"), opt("decline", "Decline", "reject_once"), opt("cancel", "Cancel", "reject_always")],
        },
        "execCommandApproval" | "applyPatchApproval" => Card {
            title: if method == "execCommandApproval" {
                format!(
                    "Run {}",
                    short(&p.get("command").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default(), 120)
                )
            } else {
                "Apply a patch".into()
            },
            text: reason.trim().to_string(),
            options: vec![
                opt("approved", "Allow once", "allow_once"),
                opt("approved_for_session", "Allow for this session", "allow_always"),
                opt("denied", "Deny", "reject_once"),
                opt("abort", "Deny and stop", "reject_always"),
            ],
        },
        _ => return None,
    })
}

/// `ToolRequestUserInputResponse` from `{questionId: [string]}` (cancel → `{answers: {}}`).
fn question_response(answers: Option<Map<String, Value>>) -> Value {
    let mut out = Map::new();
    for (qid, v) in answers.unwrap_or_default() {
        let list: Vec<Value> = match v {
            Value::Array(a) => a.into_iter().filter(|x| x.is_string()).collect(),
            Value::String(s) => vec![Value::String(s)],
            _ => vec![],
        };
        out.insert(qid, json!({ "answers": list }));
    }
    json!({ "answers": out })
}

/// Response for a decided card, in the exact schema shape of the request's method.
fn response_for(method: &str, p: &Value, choice: Option<&str>) -> Value {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({ "decision": choice.unwrap_or("cancel") })
        }
        "item/permissions/requestApproval" => match choice {
            Some(scope @ ("turn" | "session")) => {
                let mut granted = Map::new();
                if let Some(req) = p.get("permissions").and_then(Value::as_object) {
                    for (key, v) in req {
                        if !v.is_null() {
                            granted.insert(key.clone(), v.clone());
                        }
                    }
                }
                json!({ "permissions": granted, "scope": scope })
            }
            _ => json!({ "permissions": {}, "scope": "turn" }),
        },
        "item/tool/requestUserInput" => {
            let mut answers = Map::new();
            if let Some(qs) = p.get("questions").and_then(Value::as_array) {
                for (i, q) in qs.iter().enumerate() {
                    let Some(id) = q.get("id").and_then(Value::as_str) else { continue };
                    let a: Vec<String> = match (i, choice.and_then(|c| c.strip_prefix("answer:"))) {
                        (0, Some(label)) => vec![label.to_string()],
                        _ => vec![],
                    };
                    answers.insert(id.into(), json!({ "answers": a }));
                }
            }
            json!({ "answers": answers })
        }
        "mcpServer/elicitation/request" => json!({ "action": choice.unwrap_or("cancel"), "content": null, "_meta": null }),
        "execCommandApproval" | "applyPatchApproval" => {
            let d = match choice {
                Some("approved") => json!("approved"),
                Some("approved_for_session") => json!("approved_for_session"),
                Some("denied") => json!({ "denied": { "rejection": "The user denied this in OpenApp." } }),
                _ => json!("abort"),
            };
            json!({ "decision": d })
        }
        _ => Value::Null,
    }
}

fn handle_request(k: &Arc<Kernel>, c: &Arc<CodexConn>, id: Value, method: &str, p: Value) {
    if method == "item/tool/call" {
        // OpenApp registers no dynamic tools.
        c.rpc.respond(id, json!({ "contentItems": [], "success": false }));
        return;
    }
    if method == "item/tool/requestUserInput" {
        let thread = p.get("threadId").and_then(Value::as_str).unwrap_or("");
        let oa = c.session_of(thread).unwrap_or_else(|| k.active_session());
        let questions: Vec<Value> = p["questions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|q| json!({ "id": q["id"], "header": q["header"], "question": q["question"], "options": q["options"], "isOther": q["isOther"], "isSecret": q["isSecret"] }))
            .collect();
        let (_, rx) = k.ask_questions(&oa, questions, "codex");
        let c = c.clone();
        tokio::spawn(async move {
            let answers = rx.await.unwrap_or(None);
            c.rpc.respond(id, question_response(answers));
        });
        return;
    }
    let Some(card) = card_for(method, &p) else {
        c.rpc.respond_error(id, -32601, &format!("OpenApp does not support {method}"));
        return;
    };
    let thread = p.get("threadId").or_else(|| p.get("conversationId")).and_then(Value::as_str).unwrap_or("");
    let oa = c.session_of(thread).unwrap_or_else(|| k.active_session());
    let options: Vec<Value> = card.options.iter().map(|(i, n, kd)| json!({ "optionId": i, "name": n, "kind": kd })).collect();
    let (item_id, rx) = crate::consent::open(k, &oa, json!({ "title": card.title, "text": card.text, "options": options }), false);
    k.research.log("agent", Kind::Permission, json!({ "phase": "shown", "session": oa, "id": item_id, "method": method, "title": card.title }));
    let k = k.clone();
    let c = c.clone();
    let method = method.to_string();
    let started = Instant::now();
    tokio::spawn(async move {
        let choice = rx.await.unwrap_or(None);
        c.rpc.respond(id, response_for(&method, &p, choice.as_deref()));
        crate::consent::close(&k, &oa, &item_id, &choice);
        k.research.log(
            "user",
            Kind::Permission,
            json!({ "phase": "decided", "session": oa, "id": item_id, "option": choice, "ms": started.elapsed().as_millis() as u64 }),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_match_schema() {
        let p = json!({ "permissions": { "network": { "enabled": true }, "fileSystem": null } });
        assert_eq!(response_for("item/permissions/requestApproval", &p, Some("session")), json!({ "permissions": { "network": { "enabled": true } }, "scope": "session" }));
        assert_eq!(response_for("item/commandExecution/requestApproval", &p, None), json!({ "decision": "cancel" }));
        assert!(response_for("execCommandApproval", &p, Some("denied"))["decision"]["denied"]["rejection"].is_string());
        let q = json!({ "questions": [{ "id": "q1", "options": [{ "label": "Yes", "description": "" }] }, { "id": "q2" }] });
        assert_eq!(response_for("item/tool/requestUserInput", &q, Some("answer:Yes")), json!({ "answers": { "q1": { "answers": ["Yes"] }, "q2": { "answers": [] } } }));
        assert!(card_for("account/chatgptAuthTokens/refresh", &json!({})).is_none());
    }

    #[test]
    fn failed_item_text() {
        assert_eq!(item_failure_text(&json!({ "type": "mcpToolCall", "status": "failed", "error": { "message": "tool not found" } })), "tool not found");
        let r = json!({ "type": "mcpToolCall", "status": "failed", "result": { "content": [{ "type": "text", "text": "bad args" }] } });
        assert_eq!(item_failure_text(&r), "bad args");
        assert_eq!(item_failure_text(&json!({ "type": "dynamicToolCall", "status": "failed", "contentItems": [{ "type": "inputText", "text": "boom" }] })), "boom");
        assert_eq!(item_failure_text(&json!({ "type": "fileChange", "status": "declined" })), "fileChange declined");
    }
}
