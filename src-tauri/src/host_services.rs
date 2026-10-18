//! macOS host intelligence (contract §18, §18.1): the `openapp-macos` sidecar (standard library:
//! Speech, FoundationModels, Vision, AVFAudio, Translation, NaturalLanguage) and agent-written host
//! modules. Both speak JSON-RPC 2.0 over stdio (one JSON object per line). Free class: no consent
//! for the standard library; research-logged as `host_service` (method, ms, sizes, never content).

use crate::rpc::{self, Incoming, JsonRpc, RpcErr};
use crate::util::MutexExt;
use crate::modules::{self, Tail};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Standard-library methods (§18).
pub const STD_METHODS: &[&str] =
    &["speech.transcribe", "ai.generate", "ai.status", "vision.ocr", "tts.speak", "tts.render", "translate", "nl.embed", "nl.language", "status"];

/// MCP tool name of a standard-library method: `host_<method with . → _>`.
pub fn tool_name(method: &str) -> String {
    crate::primitives::agents::tool_name(&format!("host:{method}"))
}

/// The method behind an MCP tool `host_…`. `ai.status` and `status` have no tool of their own: the `host_status`
/// tool covers both.
pub fn std_tool_method(tool: &str) -> Option<&'static str> {
    STD_METHODS.iter().copied().filter(|m| !matches!(*m, "ai.status" | "status")).find(|m| tool_name(m) == tool)
}

/// Params that are file paths (mapped workspace ↔ host; refused outside the workspace).
pub const PATH_PARAMS: &[&str] = &["path"];

/// The `openapp-macos` binary: the one inside the app bundle (`Contents/MacOS/openapp-macos`); debug builds
/// also take `OPENAPP_MACOS_SERVICES` and the one build.rs built (see `bundle.rs`).
pub fn std_binary() -> Option<PathBuf> {
    crate::bundle::macos_services()
}

struct Proc {
    rpc: JsonRpc,
    alive: AtomicBool,
    pid: Option<u32>,
}

/// A supervised JSON-RPC stdio sidecar; (re)spawned on demand, at most 5 restarts per minute.
pub struct Sidecar {
    pub name: String,
    pub bin: PathBuf,
    log_path: PathBuf,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    pub tail: Tail,
    cur: Mutex<Option<Arc<Proc>>>,
    starts: Mutex<VecDeque<Instant>>,
    spawn_lock: tokio::sync::Mutex<()>,
    /// §21 G1: JSON-RPC notifications from the process (`{method, params}` without id), e.g. `channel.publish`.
    on_notify: NotifyCb,
    /// §18.1 host modules outside an app bundle: the process is responsible for itself (TCC reads its own plist).
    own_responsibility: AtomicBool,
    /// Imported frameworks whose first use can show a macOS privacy prompt (named in timeout errors).
    tcc_frameworks: Mutex<Vec<String>>,
}

/// Stderr logs of the sidecars rotate to `<log>.1` at this size.
pub const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// The handler, and notifications that arrived before one was set (kept, at most 256).
type NotifyFn = Arc<dyn Fn(&str, Value) + Send + Sync>;
type NotifyCb = Arc<Mutex<(Option<NotifyFn>, Vec<(String, Value)>)>>;

impl Sidecar {
    pub fn new(name: &str, bin: PathBuf, log_path: PathBuf, env: Vec<(String, String)>, cwd: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            bin,
            log_path,
            env,
            cwd,
            tail: Arc::new(Mutex::new(VecDeque::new())),
            cur: Mutex::new(None),
            starts: Mutex::new(VecDeque::new()),
            spawn_lock: tokio::sync::Mutex::new(()),
            on_notify: Arc::new(Mutex::new((None, Vec::new()))),
            own_responsibility: AtomicBool::new(false),
            tcc_frameworks: Mutex::new(Vec::new()),
        })
    }

    /// Host modules: TCC uses the usage descriptions embedded in the binary (not the launcher's) when the kernel
    /// does not run inside OpenApp.app (dev runs, tests, the probes harness).
    pub fn set_own_responsibility(&self) {
        self.own_responsibility.store(true, Ordering::Relaxed);
    }

    /// Host modules: the privacy-prompt frameworks the module imports ([`crate::host_modules::tcc_frameworks`]).
    pub fn set_tcc_frameworks(&self, frameworks: Vec<String>) {
        *self.tcc_frameworks.locked() = frameworks;
    }

    /// Handle notifications the process sends (method, params).
    pub fn set_on_notify(&self, f: NotifyFn) {
        let early = {
            let mut g = self.on_notify.locked();
            g.0 = Some(f.clone());
            std::mem::take(&mut g.1)
        };
        for (m, p) in early {
            f(&m, p);
        }
    }

    /// Send a JSON-RPC notification to the running process (false when it is not running).
    pub fn notify(&self, method: &str, params: Value) -> bool {
        let p = self.cur.locked().clone().filter(|p| p.alive.load(Ordering::Relaxed));
        p.is_some_and(|p| p.rpc.notify(method, Some(params)))
    }

    /// Pid of the running process.
    pub fn pid(&self) -> Option<u32> {
        self.cur.locked().as_ref().filter(|p| p.alive.load(Ordering::Relaxed)).and_then(|p| p.pid)
    }

    pub fn is_running(&self) -> bool {
        self.cur.locked().as_ref().is_some_and(|p| p.alive.load(Ordering::Relaxed))
    }

    async fn proc(&self) -> Result<Arc<Proc>, String> {
        if let Some(p) = self.cur.locked().clone().filter(|p| p.alive.load(Ordering::Relaxed)) {
            return Ok(p);
        }
        let _g = self.spawn_lock.lock().await;
        if let Some(p) = self.cur.locked().clone().filter(|p| p.alive.load(Ordering::Relaxed)) {
            return Ok(p);
        }
        {
            let mut s = self.starts.locked();
            while s.front().is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
                s.pop_front();
            }
            if s.len() >= 5 {
                let last = modules::tail_lines(&self.tail, 5).join("\n");
                return Err(format!("{} crashed 5 times in a minute; not restarting yet. Last stderr:\n{last}", self.name));
            }
            s.push_back(Instant::now());
        }
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cmd.envs(self.env.iter().cloned());
        crate::harness::sanitize_env(&mut cmd);
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        #[cfg(target_os = "macos")]
        if self.own_responsibility.load(Ordering::Relaxed) && !crate::host_modules::in_app_bundle() {
            crate::host_modules::exec_responsible_for_itself(&mut cmd, &self.bin);
        }
        let mut child = cmd.spawn().map_err(|e| format!("could not start {} ({}): {e}", self.name, self.bin.display()))?;
        {
            use std::io::Write;
            modules::rotate_log(&self.log_path, LOG_MAX_BYTES);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_path) {
                let _ = writeln!(f, "=== {} start: {} ===", crate::state::now_ts(), self.bin.display());
            }
        }
        if let Some(se) = child.stderr.take() {
            modules::pump_stderr_capped(se, self.log_path.clone(), self.tail.clone(), Some(LOG_MAX_BYTES));
        }
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let p = Arc::new(Proc { rpc: JsonRpc::new(rpc::writer(stdin), true, &self.name), alive: AtomicBool::new(true), pid: child.id() });
        let p2 = p.clone();
        let tail = self.tail.clone();
        let name = self.name.clone();
        let on_notify = self.on_notify.clone();
        tokio::spawn(async move {
            rpc::read_lines(stdout, &tail, |v| {
                // §21 G1: notifications go to the handler (kept until one is set); requests are not served
                if let Some(Incoming::Notification { method, params }) = p2.rpc.route(v) {
                    let cb = {
                        let mut g = on_notify.locked();
                        if g.0.is_none() && g.1.len() < 256 {
                            g.1.push((method.clone(), params.clone()));
                        }
                        g.0.clone()
                    };
                    if let Some(cb) = cb {
                        cb(&method, params);
                    }
                }
            })
            .await;
            let status = child.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            p2.alive.store(false, Ordering::Relaxed);
            p2.rpc.fail_pending(&format!("{name} exited ({status})"));
            log::warn!("{name} exited ({status})");
        });
        *self.cur.locked() = Some(p.clone());
        Ok(p)
    }

    /// One JSON-RPC call (spawns or restarts the process when needed).
    pub async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let p = self.proc().await?;
        p.rpc.call(method, params, timeout).await.map_err(|e| match e {
            RpcErr::Remote(e) => e.get("message").and_then(Value::as_str).unwrap_or("error").to_string(),
            RpcErr::Closed => format!("{} closed the connection", self.name),
            RpcErr::Timeout => self.timeout_message(method, timeout),
        })
    }

    fn timeout_message(&self, method: &str, timeout: Duration) -> String {
        let tcc = self.tcc_frameworks.locked().clone();
        if tcc.is_empty() {
            return format!("{method} timed out after {}s", timeout.as_secs());
        }
        let log = self.log_path.file_name().map(|f| format!(" Its stderr: .openapp/logs/{}.", f.to_string_lossy())).unwrap_or_default();
        format!(
            "{method}: the host module did not answer in {} s — macOS may be showing a permission prompt to the user (it imports {}). Ask the user to answer the macOS dialog, then call again.{log}",
            timeout.as_secs(),
            tcc.join(", ")
        )
    }

    pub fn kill(&self) {
        if let Some(p) = self.cur.locked().take() {
            p.alive.store(false, Ordering::Relaxed);
            if let Some(pid) = p.pid {
                let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
            }
        }
    }
}

/// Timeout per method (FoundationModels and Speech can take a while on a cold start; asset download).
pub fn timeout_for(method: &str) -> Duration {
    match method {
        "speech.transcribe" => Duration::from_secs(300),
        "ai.generate" | "tts.speak" | "translate" => Duration::from_secs(180),
        _ => Duration::from_secs(60),
    }
}
