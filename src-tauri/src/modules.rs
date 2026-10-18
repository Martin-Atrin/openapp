//! Module manifests, views and MCP client processes (contract §3).

use crate::util::MutexExt;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::transport::TokioChildProcess;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncBufReadExt;

pub type ModClient = rmcp::service::RunningService<rmcp::RoleClient, ()>;
pub type Tail = Arc<Mutex<VecDeque<String>>>;

const TAIL_CAP: usize = 200;
pub const INIT_TIMEOUT: Duration = Duration::from_secs(20);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// `"net": "socks"`: in the VM the server runs under proxychains4 (all TCP through the SOCKS5 egress)
    pub socks: bool,
}

#[derive(Debug, Clone)]
pub struct ViewSpec {
    pub kind: String,
    /// spec/html: file inside the module; web: empty
    pub file: String,
    /// web: start URL
    pub url: Option<String>,
    /// web: persistent browser profile (default: module id)
    pub profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DefaultPlacement {
    pub view: String,
    pub anchor: String,
    pub label: Option<String>,
    pub order: Option<f64>,
}

/// §18.1 host part: Swift compiled and run on the Mac (consent per source hash).
#[derive(Debug, Clone)]
pub struct HostSpec {
    /// single-file / folder build: a .swift file; all .swift files in its folder are compiled
    pub swift: Option<String>,
    /// SwiftPM package folder (with `product`, default = module id)
    pub package: Option<String>,
    pub product: Option<String>,
    /// one-sentence, agent-written summary for the consent card
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub server: Option<ServerSpec>,
    pub host: Option<HostSpec>,
    pub views: BTreeMap<String, ViewSpec>,
    pub placements: Vec<DefaultPlacement>,
    /// The whole openapp.json object (§21 primitives read their own keys: channels, activation, services, …).
    pub raw: Value,
}

pub fn valid_module_id(id: &str) -> bool {
    crate::util::valid_slug(id, 64)
}

/// A relative path inside a module: no absolute paths, no `..`.
pub fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && p.split('/').all(|seg| !seg.is_empty() && seg != ".." && seg != ".")
}

/// Read and validate `modules/<id>/openapp.json`.
pub fn read_manifest(dir: &Path, folder_id: &str) -> Result<Manifest, Vec<String>> {
    let mut p = Vec::new();
    if !valid_module_id(folder_id) {
        return Err(vec![format!(
            "folder name \"{folder_id}\" is not a valid module id; use [a-z0-9-]+ (rename the folder)"
        )]);
    }
    let path = dir.join("openapp.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return Err(vec![format!(
                "modules/{folder_id}/openapp.json not found; create it ({{\"id\":\"{folder_id}\",\"name\":...,\"views\":{{...}}}})"
            )]);
        }
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return Err(vec![format!(
                "modules/{folder_id}/openapp.json: invalid JSON at line {} column {}: {e}",
                e.line(),
                e.column()
            )]);
        }
    };
    let Some(o) = v.as_object() else {
        return Err(vec!["openapp.json must be a JSON object".into()]);
    };
    let id = o.get("id").and_then(Value::as_str).unwrap_or("");
    if id != folder_id {
        p.push(format!(
            "\"id\" is {} but the folder is modules/{folder_id}; they must be equal",
            o.get("id").map(|v| v.to_string()).unwrap_or_else(|| "missing".into())
        ));
    }
    let name = match o.get("name") {
        None => folder_id.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(v) => {
            p.push(format!("\"name\" must be a non-empty string, got {v}"));
            folder_id.to_string()
        }
    };
    let server = match o.get("server") {
        None | Some(Value::Null) => None,
        Some(Value::Object(s)) => {
            let command: Vec<String> = match s.get("command") {
                Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) => {
                    a.iter().map(|x| x.as_str().unwrap().to_string()).collect()
                }
                Some(Value::String(_)) => {
                    p.push("server.command must be an array of strings, e.g. [\"node\", \"server.ts\"], not a single string".into());
                    vec![]
                }
                _ => {
                    p.push("server.command must be a non-empty array of strings, e.g. [\"node\", \"server.ts\"]".into());
                    vec![]
                }
            };
            let mut env = BTreeMap::new();
            match s.get("env") {
                None | Some(Value::Null) => {}
                Some(Value::Object(e)) => {
                    for (k, v) in e {
                        match v.as_str() {
                            Some(s) => {
                                env.insert(k.clone(), s.to_string());
                            }
                            None => p.push(format!("server.env.{k} must be a string")),
                        }
                    }
                }
                Some(_) => p.push("server.env must be an object of strings".into()),
            }
            let socks = match s.get("net").and_then(Value::as_str) {
                None => false,
                Some("socks") => true,
                Some(n) => {
                    p.push(format!("server.net must be \"socks\" (all TCP through OpenApp's SOCKS proxy) or omitted, got \"{n}\""));
                    false
                }
            };
            Some(ServerSpec { command, env, socks })
        }
        Some(_) => {
            p.push("\"server\" must be an object { \"command\": [...], \"env\"?: {...} }".into());
            None
        }
    };
    let host = match o.get("host") {
        None | Some(Value::Null) => None,
        Some(Value::Object(h)) => {
            let get = |k: &str| h.get(k).and_then(Value::as_str).map(String::from);
            let (swift, package) = (get("swift"), get("package"));
            match (&swift, &package) {
                (Some(f), None) if safe_rel(f) && f.ends_with(".swift") => {}
                (None, Some(d)) if safe_rel(d) => {}
                _ => p.push("\"host\" needs exactly one of \"swift\": \"host/main.swift\" (a .swift file inside the module) or \"package\": \"host\" (a SwiftPM folder)".into()),
            }
            if get("summary").is_none_or(|s| s.trim().is_empty()) {
                p.push("\"host.summary\" is required: one sentence for the user's consent card (what it does on their Mac)".into());
            }
            Some(HostSpec { swift, package, product: get("product"), summary: get("summary") })
        }
        Some(_) => {
            p.push("\"host\" must be an object { \"swift\": \"host/main.swift\", \"summary\": \"...\" }".into());
            None
        }
    };
    let mut views = BTreeMap::new();
    match o.get("views") {
        None | Some(Value::Null) => {}
        Some(Value::Object(vs)) => {
            for (vname, spec) in vs {
                if !crate::layout::valid_ident(vname) {
                    p.push(format!("views.{vname}: view names must match [A-Za-z0-9_-]+"));
                    continue;
                }
                let kind = spec.get("kind").and_then(Value::as_str).unwrap_or("");
                let file = spec.get("file").and_then(Value::as_str).unwrap_or("");
                if kind == "web" {
                    let url = spec.get("url").and_then(Value::as_str).unwrap_or("");
                    if !(url.starts_with("https://") || url.starts_with("http://")) {
                        p.push(format!("views.{vname}.url must be an http(s) URL for a web view"));
                        continue;
                    }
                    let profile = match spec.get("profile") {
                        None | Some(Value::Null) => folder_id.to_string(),
                        Some(Value::String(s)) if crate::layout::valid_ident(s) => s.clone(),
                        Some(v) => {
                            p.push(format!("views.{vname}.profile must match [A-Za-z0-9_-]+, got {v}"));
                            continue;
                        }
                    };
                    views.insert(
                        vname.clone(),
                        ViewSpec { kind: "web".into(), file: String::new(), url: Some(url.into()), profile: Some(profile) },
                    );
                    continue;
                }
                if kind != "spec" && kind != "html" {
                    p.push(format!("views.{vname}.kind must be \"spec\", \"html\" or \"web\""));
                    continue;
                }
                if !safe_rel(file) {
                    p.push(format!(
                        "views.{vname}.file must be a relative path inside the module (no \"..\"), e.g. \"views/{vname}.{}\"",
                        if kind == "spec" { "json" } else { "html" }
                    ));
                    continue;
                }
                views.insert(vname.clone(), ViewSpec { kind: kind.into(), file: file.into(), url: None, profile: None });
            }
        }
        Some(_) => p.push("\"views\" must be an object { name: { kind, file } }".into()),
    }
    let mut placements = Vec::new();
    match o.get("placements") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for (i, pl) in a.iter().enumerate() {
                let view = pl.get("view").and_then(Value::as_str).unwrap_or("");
                let anchor = pl.get("anchor").and_then(Value::as_str).unwrap_or("");
                if !views.contains_key(view) {
                    p.push(format!("placements[{i}].view \"{view}\" is not in \"views\""));
                    continue;
                }
                if anchor.is_empty() {
                    p.push(format!("placements[{i}].anchor is missing"));
                    continue;
                }
                placements.push(DefaultPlacement {
                    view: view.into(),
                    anchor: anchor.into(),
                    label: pl.get("label").and_then(Value::as_str).map(String::from),
                    order: pl.get("order").and_then(Value::as_f64),
                });
            }
        }
        Some(_) => p.push("\"placements\" must be an array".into()),
    }
    if !p.is_empty() {
        return Err(p.into_iter().map(|x| format!("modules/{folder_id}/openapp.json: {x}")).collect());
    }
    Ok(Manifest { id: folder_id.into(), name, server, host, views, placements, raw: v.clone() })
}

/// Peek at a manifest's name without full validation (for stopped modules).
pub fn peek_name(dir: &Path, id: &str) -> String {
    std::fs::read_to_string(dir.join("openapp.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("name").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| id.to_string())
}

#[derive(Debug, Clone)]
pub struct ViewEntry {
    pub kind: String,
    pub file: String,
    /// Snapshot value: {kind:"spec",tree} | {kind:"html",url,rev} | {kind:"error",error}
    pub value: Value,
    pub problems: Vec<String>,
    pub rev: u64,
    pub mtime: Option<SystemTime>,
}

impl ViewEntry {
    pub fn tree(&self) -> Option<&Value> {
        if self.value.get("kind").and_then(Value::as_str) == Some("spec") { self.value.get("tree") } else { None }
    }
}

pub fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

pub fn read_view(
    dir: &Path,
    module: &str,
    spec: &ViewSpec,
    tools: Option<&BTreeSet<String>>,
    rev: u64,
) -> ViewEntry {
    if spec.kind == "web" {
        return ViewEntry {
            kind: "web".into(),
            file: String::new(),
            value: json!({ "kind": "web", "url": spec.url, "profile": spec.profile }),
            problems: Vec::new(),
            rev,
            mtime: None,
        };
    }
    let path = dir.join(&spec.file);
    let mt = mtime(&path);
    let mut entry = ViewEntry {
        kind: spec.kind.clone(),
        file: spec.file.clone(),
        value: Value::Null,
        problems: Vec::new(),
        rev,
        mtime: mt,
    };
    if mt.is_none() {
        entry.value = json!({ "kind": "error", "error": format!("modules/{module}/{} not found", spec.file) });
        return entry;
    }
    if spec.kind == "html" {
        entry.value = json!({ "kind": "html", "url": format!("oamod://{module}/{}", spec.file), "rev": rev });
        return entry;
    }
    match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| {
        serde_json::from_str::<Value>(&t)
            .map_err(|e| format!("invalid JSON at line {} column {}: {e}", e.line(), e.column()))
    }) {
        Ok(tree) => {
            entry.problems = crate::views::validate(&tree, tools)
                .into_iter()
                .map(|p| format!("modules/{module}/{}: {p}", spec.file))
                .collect();
            entry.value = json!({ "kind": "spec", "tree": tree });
        }
        Err(e) => {
            entry.value = json!({ "kind": "error", "error": format!("modules/{module}/{}: {e}", spec.file) });
        }
    }
    entry
}

/// Files of a module, for change detection (skips node_modules, data, dot dirs).
pub fn scan_files(dir: &Path) -> BTreeMap<PathBuf, SystemTime> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" || name == "data" || name == "target" {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < 5 {
                    stack.push((e.path(), depth + 1));
                }
            } else if let Some(m) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                out.insert(e.path(), m);
                if out.len() >= 2000 {
                    return out;
                }
            }
        }
    }
    out
}

pub fn which(cmd: &str, path_env: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        let p = PathBuf::from(cmd);
        return p.is_file().then_some(p);
    }
    for dir in std::env::split_paths(path_env) {
        let cand = dir.join(cmd);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file() || p.with_extension("exe").is_file() || p.with_extension("cmd").is_file()
    }
}

pub fn tail_lines(t: &Tail, n: usize) -> Vec<String> {
    let g = t.locked();
    g.iter().skip(g.len().saturating_sub(n)).cloned().collect()
}

pub fn push_tail(t: &Tail, line: String) {
    let mut g = t.locked();
    if g.len() >= TAIL_CAP {
        g.pop_front();
    }
    g.push_back(line);
}

/// Pump a child's stderr into a log file and a tail buffer.
pub fn pump_stderr<R: tokio::io::AsyncRead + Unpin + Send + 'static>(stderr: R, log_path: PathBuf, tail: Tail) {
    pump_stderr_capped(stderr, log_path, tail, None);
}

/// `<log>.1` for a rotated log.
fn rotated(log_path: &Path) -> PathBuf {
    let mut s = log_path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Rotate `log_path` to `<log>.1` (replacing an older one) when it holds `max` bytes or more. True when rotated.
pub fn rotate_log(log_path: &Path, max: u64) -> bool {
    std::fs::metadata(log_path).is_ok_and(|m| m.len() >= max) && std::fs::rename(log_path, rotated(log_path)).is_ok()
}

/// [`pump_stderr`] that keeps the log under `max` bytes: the full file moves to `<log>.1` and a new one starts.
/// Several writers may share one log (another writer's rotation is followed by reopening the path).
pub fn pump_stderr_capped<R: tokio::io::AsyncRead + Unpin + Send + 'static>(stderr: R, log_path: PathBuf, tail: Tail, max: Option<u64>) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let open = |p: PathBuf| async move { tokio::fs::OpenOptions::new().create(true).append(true).open(p).await.ok() };
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let mut file = open(log_path.clone()).await;
        let mut written = size(&log_path);
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
                written += line.len() as u64 + 1;
            }
            if let Some(max) = max
                && written >= max
            {
                // rotate when the path is full; reopen in any case (another writer may have rotated it)
                if let Some(f) = file.as_mut() {
                    let _ = f.flush().await;
                }
                rotate_log(&log_path, max);
                file = open(log_path.clone()).await;
                written = size(&log_path);
            }
            push_tail(&tail, line);
        }
    });
}

/// Host pids of module servers started on the Mac, by module folder (§21 G9 resource sampling).
pub fn host_pids() -> &'static Mutex<std::collections::HashMap<PathBuf, u32>> {
    static P: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, u32>>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

/// Start a module server and list its tools.
pub async fn start_server(
    dir: &Path,
    id: &str,
    spec: &ServerSpec,
    path_env: &str,
    log_path: PathBuf,
    tail: Tail,
    extra_env: &[(String, String)],
) -> Result<(Arc<ModClient>, Vec<Tool>), String> {
    let prog = &spec.command[0];
    let resolved = if prog.contains('/') {
        let p = if Path::new(prog).is_absolute() { PathBuf::from(prog) } else { dir.join(prog) };
        p.is_file().then_some(p)
    } else {
        which(prog, path_env)
    };
    let Some(resolved) = resolved else {
        return Err(format!(
            "server.command[0] \"{prog}\" was not found on PATH. Use a runtime that openapp_describe lists as available (e.g. node), or a path relative to the module folder."
        ));
    };
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(f, "=== {} start: {} ===", crate::state::now_ts(), spec.command.join(" "));
        }
    }
    let mut cmd = tokio::process::Command::new(&resolved);
    cmd.args(&spec.command[1..])
        .current_dir(dir)
        .env("PATH", path_env)
        .env("OPENAPP_MODULE", id)
        .envs(extra_env.iter().cloned())
        .envs(&spec.env)
        .kill_on_drop(true);
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", spec.command.join(" ")))?;
    if let Some(se) = stderr {
        pump_stderr(se, log_path, tail.clone());
    }
    if let Some(pid) = transport.id() {
        host_pids().locked().insert(dir.to_path_buf(), pid);
    }
    let client = match tokio::time::timeout(INIT_TIMEOUT, ().serve(transport)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(format!("MCP initialize failed: {e}")),
        Err(_) => {
            return Err(format!(
                "server did not answer MCP initialize within {}s (is it an MCP stdio server? never print to stdout)",
                INIT_TIMEOUT.as_secs()
            ));
        }
    };
    let tools = match tokio::time::timeout(Duration::from_secs(15), client.list_all_tools()).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(format!("tools/list failed: {e}")),
        Err(_) => return Err("tools/list timed out after 15s".into()),
    };
    Ok((Arc::new(client), tools))
}

/// Start a module server inside the app VM (§17): exec stdio in `/workspace/modules/<id>`, same
/// MCP contract. Network goes through the kernel proxy (actor `module.<id>`).
pub async fn start_server_vm(
    vm: &Arc<crate::vm::AppVm>,
    dir: &Path,
    id: &str,
    spec: &ServerSpec,
    log_path: PathBuf,
    tail: Tail,
    extra_env: &[(String, String)],
) -> Result<(Arc<ModClient>, Vec<Tool>), String> {
    let gdir = vm.guest_path(dir).ok_or("module folder is outside the workspace")?;
    let prog = &spec.command[0];
    // resolve like the host does: PATH, or a path relative to the module folder
    let (code, found, _) = vm
        .exec_out("sh", &["-c".into(), "command -v \"$1\"".into(), "sh".into(), prog.clone()], &[], Some(&gdir))
        .await?;
    if code != 0 || found.trim().is_empty() {
        return Err(format!(
            "server.command[0] \"{prog}\" was not found in the app VM (PATH or relative to modules/{id}/). Available there: node, npm, python3, ffmpeg, git (see openapp_describe host.tools)."
        ));
    }
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(f, "=== {} start (app VM): {} ===", crate::state::now_ts(), spec.command.join(" "));
        }
    }
    let mut env: Vec<(String, String)> = crate::vm::proxy_env(&format!("module.{id}"));
    env.push(("OPENAPP_MODULE".into(), id.into()));
    env.extend(extra_env.iter().cloned());
    env.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut child = if spec.socks {
        let w = crate::vm::socks_wrap(&format!("module.{id}"), &spec.command);
        vm.spawn(&w[0], &w[1..], &env, Some(&gdir)).await?
    } else {
        vm.spawn(prog, &spec.command[1..], &env, Some(&gdir)).await?
    };
    if let Some(se) = child.stderr.take() {
        pump_stderr(se, log_path, tail.clone());
    }
    let kill = child.killer();
    let stdout = child.take_stdout();
    let stdin = child.take_stdin();
    let mut exit = child.take_exit();
    let client = match tokio::time::timeout(INIT_TIMEOUT, ().serve((stdout, stdin))).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            kill.kill();
            let why = match exit.as_mut().map(|rx| rx.try_recv()) {
                Some(Ok(info)) => format!(" (process exited {}{})", info.code, info.failed.map(|f| format!(": {f}")).unwrap_or_default()),
                _ => String::new(),
            };
            return Err(format!("MCP initialize failed: {e}{why}"));
        }
        Err(_) => {
            kill.kill();
            return Err(format!(
                "server did not answer MCP initialize within {}s (is it an MCP stdio server? never print to stdout)",
                INIT_TIMEOUT.as_secs()
            ));
        }
    };
    let tools = match tokio::time::timeout(Duration::from_secs(15), client.list_all_tools()).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            kill.kill();
            return Err(format!("tools/list failed: {e}"));
        }
        Err(_) => {
            kill.kill();
            return Err("tools/list timed out after 15s".into());
        }
    };
    let client = Arc::new(client);
    // unload / reload / kernel exit (client cancelled or dropped): stop the guest process too
    let weak = Arc::downgrade(&client);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match weak.upgrade() {
                Some(c) if !c.is_closed() => {}
                _ => {
                    kill.kill();
                    break;
                }
            }
        }
    });
    Ok((client, tools))
}

pub async fn call_tool(client: &ModClient, tool: &str, args: Map<String, Value>) -> Result<CallToolResult, String> {
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
    match tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(format!("tool call failed: {e}")),
        Err(_) => Err(format!("tool call timed out after {}s", CALL_TIMEOUT.as_secs())),
    }
}

/// Text of a CallToolResult's text content blocks.
pub fn result_text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}
