//! Hand-rolled ACP client (JSON-RPC 2.0, newline-delimited over the harness stdio). Contract §9.

use crate::research::Kind;
use crate::rpc::{self, Incoming, JsonRpc};
use crate::util::MutexExt;
use crate::kernel::Kernel;
use crate::modules::{self, Tail};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u64 = 1;

use crate::harness::{cancel_open_permissions, turn_finished, turn_started};

#[derive(Default)]
struct Turn {
    /// last chat item appended during this turn
    last_item: Option<String>,
    last_kind: String,
    plan_item: Option<String>,
    tools: HashMap<String, String>,
    text: String,
}

/// (model id, effort) last applied to an ACP session.
type ModelEffort = (Option<String>, Option<String>);

pub struct AcpConn {
    pub rpc: JsonRpc,
    /// OpenApp session id -> ACP session id (sessions opened in this harness process).
    acp_ids: Mutex<HashMap<String, String>>,
    /// agentCapabilities.loadSession
    load_session: AtomicBool,
    pub pid: Option<u32>,
    /// Harness id (key for stored session ids).
    pub harness_id: String,
    pub tail: Tail,
    /// Streaming state per OpenApp session.
    turns: Mutex<HashMap<String, Turn>>,
    /// Serializes ACP session creation.
    create_lock: tokio::sync::Mutex<()>,
    /// OpenApp sessions whose ACP session already got the containment prefix.
    primed: Mutex<std::collections::HashSet<String>>,
    /// §23 how this agent takes a model: config option id (`configOptions` category "model") or `session/set_model`.
    model_cfg: Mutex<Option<String>>,
    /// §23 config option id of the reasoning level (category "thought_level"), if advertised.
    effort_cfg: Mutex<Option<String>>,
    /// §23 ACP session id → (model, effort) currently set.
    applied: Mutex<HashMap<String, ModelEffort>>,
}

impl AcpConn {
    /// OpenApp session for an ACP session id.
    fn oa_session(&self, acp: &str) -> Option<String> {
        self.acp_ids.locked().iter().find(|(_, a)| a.as_str() == acp).map(|(o, _)| o.clone())
    }

    pub fn acp_id(&self, oa: &str) -> Option<String> {
        self.acp_ids.locked().get(oa).cloned()
    }

}

fn conn(k: &Kernel) -> Option<Arc<AcpConn>> {
    k.acp.locked().clone()
}

/// Spawn the harness, initialize, create the session.
pub async fn run(k: Arc<Kernel>, spec: crate::harness::HarnessSpec) {
    k.set_agent("starting", None);
    if k.mcp.get().is_none() {
        k.set_agent("error", Some("kernel MCP server is not running".into()));
        return;
    }
    let log_path = crate::harness::agent_log(&k, &spec.cmd);
    let mut cmd = crate::harness::host_command(&k, &spec.cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            k.set_agent("error", Some(format!("could not start agent harness `{}`: {e}", spec.cmd)));
            return;
        }
    };
    let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(se) = child.stderr.take() {
        modules::pump_stderr(se, log_path, tail.clone());
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let c = Arc::new(AcpConn {
        rpc: JsonRpc::new(rpc::writer(stdin), true, "agent harness"),
        acp_ids: Mutex::new(HashMap::new()),
        load_session: AtomicBool::new(false),
        pid: child.id(),
        harness_id: spec.id.clone(),
        tail: tail.clone(),
        turns: Mutex::new(HashMap::new()),
        create_lock: tokio::sync::Mutex::new(()),
        primed: Mutex::new(std::collections::HashSet::new()),
        model_cfg: Mutex::new(None),
        effort_cfg: Mutex::new(None),
        applied: Mutex::new(HashMap::new()),
    });
    *k.acp.locked() = Some(c.clone());

    // reader
    {
        let (k, c) = (k.clone(), c.clone());
        tokio::spawn(async move { rpc::read_lines(stdout, &c.tail, |v| handle_message(&k, &c, v)).await });
    }
    // exit watcher
    {
        let k = k.clone();
        let c = c.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.rpc.fail_pending("agent harness exited");
            let is_current = conn(&k).is_some_and(|cur| Arc::ptr_eq(&cur, &c));
            if is_current {
                *k.acp.locked() = None;
            }
            let code = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            crate::harness::report_exit(&k, is_current, "agent harness", &code, &c.tail);
        });
    }

    // initialize
    let init = c
        .rpc.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "openapp", "title": "OpenApp", "version": env!("CARGO_PKG_VERSION") }
            }),
            Duration::from_secs(300),
        )
        .await;
    let init = match init {
        Ok(v) => v,
        Err(e) => {
            let last = modules::tail_lines(&c.tail, 10).join("\n");
            k.set_agent("error", Some(format!("initialize failed: {e}\n{last}")));
            return;
        }
    };
    if let Some(info) = init.get("agentInfo")
        && let Some(n) = info.get("title").or_else(|| info.get("name")).and_then(Value::as_str) {
            k.set_agent_name(n);
        }
    let http_ok = init.pointer("/agentCapabilities/mcpCapabilities/http").and_then(Value::as_bool).unwrap_or(false);
    if !http_ok {
        log::warn!("agent does not advertise mcpCapabilities.http; trying anyway");
    }
    let can_load = init.pointer("/agentCapabilities/loadSession").and_then(Value::as_bool).unwrap_or(false);
    c.load_session.store(can_load, Ordering::Relaxed);
    // ACP sessions are created lazily, on the first prompt of each OpenApp session (§10).
    k.set_agent("ready", None);
}

fn mcp_servers(k: &Kernel) -> Value {
    match k.mcp.get() {
        Some(ep) => json!([{
            "type": "http",
            "name": "openapp",
            "url": ep.url,
            "headers": [{ "name": "Authorization", "value": format!("Bearer {}", ep.token) }]
        }]),
        None => json!([]),
    }
}

/// ACP session for an OpenApp session: reuse, `session/load` (if supported and stored), else `session/new`.
async fn ensure_session(k: &Arc<Kernel>, c: &Arc<AcpConn>, oa: &str) -> Result<String, String> {
    let _g = c.create_lock.lock().await;
    if let Some(id) = c.acp_id(oa) {
        return Ok(id);
    }
    let stored = k.agent_session_id(oa, &c.harness_id);
    if let (true, Some(acp)) = (c.load_session.load(Ordering::Relaxed), stored) {
        // History replayed during session/load is not mapped yet, so it is dropped (the chat is
        // already persisted by the kernel).
        let r = c
            .rpc.request(
                "session/load",
                json!({ "sessionId": acp, "cwd": k.workspace.to_string_lossy(), "mcpServers": mcp_servers(k) }),
                Duration::from_secs(180),
            )
            .await;
        match r {
            Ok(v) => {
                learn_models(k, c, &acp, &v);
                c.acp_ids.locked().insert(oa.to_string(), acp.clone());
                return Ok(acp);
            }
            Err(e) => log::warn!("session/load {acp} failed ({e}); creating a new ACP session"),
        }
    }
    let r = c
        .rpc.request(
            "session/new",
            json!({ "cwd": k.workspace.to_string_lossy(), "mcpServers": mcp_servers(k) }),
            Duration::from_secs(180),
        )
        .await;
    match r.as_ref().ok().and_then(|v| v.get("sessionId")).and_then(Value::as_str) {
        Some(acp) => {
            if let Ok(v) = &r {
                learn_models(k, c, acp, v);
            }
            c.acp_ids.locked().insert(oa.to_string(), acp.to_string());
            k.set_agent_session_id(oa, &c.harness_id, Some(acp.to_string()));
            Ok(acp.to_string())
        }
        None => {
            let err = match r {
                Err(e) => e,
                Ok(v) => format!("session/new returned no sessionId: {v}"),
            };
            let hint = if err.to_ascii_lowercase().contains("auth") {
                " The agent needs a login. Run the harness's login once in a terminal (for Claude Code: `claude /login`), then restart OpenApp."
            } else {
                ""
            };
            Err(format!("session/new failed: {err}{hint}"))
        }
    }
}

/// Options of an ACP select config option (flat or grouped).
fn select_options(o: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for x in o.get("options").and_then(Value::as_array).into_iter().flatten() {
        match x.get("options").and_then(Value::as_array) {
            Some(group) => out.extend(group.iter().cloned()),
            None => out.push(x.clone()),
        }
    }
    out
}

/// §23: models an ACP agent advertises in `session/new` / `session/load` (`configOptions` category "model" with a
/// "thought_level" option, else `models.availableModels`). Nothing advertised → "agent default" (empty catalog).
pub fn models_from_session(harness: &str, v: &Value) -> (Vec<crate::primitives::models::ModelInfo>, Option<String>, Option<String>, Option<String>) {
    use crate::primitives::models::ModelInfo;
    let opts = v.get("configOptions").and_then(Value::as_array).cloned().unwrap_or_default();
    let by_cat = |cat: &str| opts.iter().find(|o| o.get("category").and_then(Value::as_str) == Some(cat)).cloned();
    let efforts_opt = by_cat("thought_level");
    let efforts: Vec<String> = efforts_opt.as_ref().map(|o| select_options(o).iter().filter_map(|x| x.get("value").and_then(Value::as_str).map(String::from)).collect()).unwrap_or_default();
    let cur_effort = efforts_opt.as_ref().and_then(|o| o.get("currentValue").and_then(Value::as_str).map(String::from));
    let effort_id = efforts_opt.as_ref().and_then(|o| o.get("id").and_then(Value::as_str).map(String::from));
    let mk = |id: &str, name: Option<&str>, desc: Option<&str>, current: bool| ModelInfo {
        id: id.to_string(),
        display_name: name.filter(|n| !n.is_empty()).unwrap_or(id).to_string(),
        description: desc.unwrap_or("").to_string(),
        efforts: efforts.clone(),
        default_effort: cur_effort.clone(),
        hidden: false,
        is_default: current,
        harness: harness.to_string(),
    };
    if let Some(o) = by_cat("model") {
        let cur = o.get("currentValue").and_then(Value::as_str).unwrap_or("");
        let list = select_options(&o)
            .iter()
            .filter_map(|x| x.get("value").and_then(Value::as_str).map(|id| mk(id, x.get("name").and_then(Value::as_str), x.get("description").and_then(Value::as_str), id == cur)))
            .collect();
        return (list, o.get("id").and_then(Value::as_str).map(String::from), effort_id, Some(cur.to_string()));
    }
    let Some(m) = v.get("models") else { return (Vec::new(), None, effort_id, None) };
    let cur = m.get("currentModelId").and_then(Value::as_str).unwrap_or("");
    let list = m
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.get("modelId").and_then(Value::as_str).map(|id| mk(id, x.get("name").and_then(Value::as_str), x.get("description").and_then(Value::as_str), id == cur)))
        .collect();
    (list, None, effort_id, Some(cur.to_string()))
}

fn learn_models(k: &Arc<Kernel>, c: &Arc<AcpConn>, acp: &str, v: &Value) {
    let (list, model_cfg, effort_cfg, current) = models_from_session(&c.harness_id, v);
    *c.effort_cfg.locked() = effort_cfg;
    if list.is_empty() {
        return;
    }
    *c.model_cfg.locked() = model_cfg;
    let effort = list.first().and_then(|m| m.default_effort.clone());
    c.applied.locked().insert(acp.to_string(), (current, effort));
    let changed = crate::primitives::models::catalog(k, &c.harness_id).iter().map(|m| &m.id).collect::<Vec<_>>() != list.iter().map(|m| &m.id).collect::<Vec<_>>();
    if changed {
        crate::primitives::models::set_catalog(k, &c.harness_id, list, "acp-session", None);
    }
}

/// §23: set the resolved model/effort on the ACP session before a prompt (config option or `session/set_model`).
async fn apply_choice(k: &Arc<Kernel>, c: &Arc<AcpConn>, oa: &str, acp: &str, mode: &str) -> Option<crate::primitives::models::Resolved> {
    let r = crate::primitives::models::resolve(k, &c.harness_id, mode, Some(oa))?;
    let (cur_m, cur_e) = c.applied.locked().get(acp).cloned().unwrap_or_default();
    let model_cfg = c.model_cfg.locked().clone();
    if cur_m.as_deref() != Some(r.model.id.as_str()) {
        let res = match &model_cfg {
            Some(id) => c.rpc.request("session/set_config_option", json!({ "sessionId": acp, "configId": id, "value": r.model.id }), Duration::from_secs(30)).await,
            None => c.rpc.request("session/set_model", json!({ "sessionId": acp, "modelId": r.model.id }), Duration::from_secs(30)).await,
        };
        match res {
            Ok(_) => {
                c.applied.locked().entry(acp.to_string()).or_default().0 = Some(r.model.id.clone());
            }
            Err(e) => log::warn!("acp: setting model {} failed: {e}", r.model.id),
        }
    }
    let effort_cfg = c.effort_cfg.locked().clone();
    if let (Some(cfg), Some(e)) = (effort_cfg, &r.effort)
        && cur_e.as_deref() != Some(e.as_str()) {
            match c.rpc.request("session/set_config_option", json!({ "sessionId": acp, "configId": cfg, "value": e }), Duration::from_secs(30)).await {
                Ok(_) => c.applied.locked().entry(acp.to_string()).or_default().1 = Some(e.clone()),
                Err(err) => log::warn!("acp: setting effort {e} failed: {err}"),
            }
        }
    k.research.log("kernel", Kind::TurnModel, json!({ "session": oa, "harness": c.harness_id, "model": r.model.id, "effort": r.effort, "tier": r.tier }));
    Some(r)
}

/// Prompt blocks: containment + mode rule on the first prompt of each ACP session (§12.3),
/// the build-mode prefix on build turns (§12.1), then the user's text.
fn prompt_blocks(k: &Kernel, c: &AcpConn, oa: &str, text: &str, build: bool) -> Value {
    let mut blocks = Vec::new();
    if c.primed.locked().insert(oa.to_string()) {
        blocks.push(json!({ "type": "text", "text": format!("{}\n\n{}", crate::harness::CONTAINMENT, crate::harness::MODE_RULE) }));
    }
    if build {
        blocks.push(json!({ "type": "text", "text": crate::harness::acp_build_prefix(k) }));
    }
    blocks.push(json!({ "type": "text", "text": text }));
    Value::Array(blocks)
}

/// Drop the ACP mapping of an OpenApp session (after /reset): the next prompt starts fresh.
pub fn forget_session(k: &Kernel, oa: &str) {
    if let Some(c) = conn(k) {
        c.acp_ids.locked().remove(oa);
        c.primed.locked().remove(oa);
    }
}

fn handle_message(k: &Arc<Kernel>, c: &Arc<AcpConn>, v: Value) {
    match c.rpc.route(v) {
        Some(Incoming::Request { id, method, params }) => handle_request(k, c, id, &method, params),
        Some(Incoming::Notification { method, params }) if method == "session/update" => {
            let acp = params.get("sessionId").and_then(Value::as_str).unwrap_or("");
            // updates for unknown ACP sessions (e.g. history replay during session/load) are dropped
            if let (Some(oa), Some(u)) = (c.oa_session(acp), params.get("update")) {
                handle_update(k, c, &oa, u);
            }
        }
        _ => {}
    }
}

fn handle_request(k: &Arc<Kernel>, c: &Arc<AcpConn>, id: Value, method: &str, params: Value) {
    if method == "elicitation/create" || method == "session/elicitation" {
        handle_elicitation(k, c, id, &params);
        return;
    }
    if method != "session/request_permission" {
        c.rpc.respond_error(id, -32601, &format!("Method not found: {method}"));
        return;
    }
    let oa = params
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(|a| c.oa_session(a))
        .unwrap_or_else(|| k.active_session());
    let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
    let title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            let tid = tool_call.get("toolCallId").and_then(Value::as_str)?;
            let item = c.turns.lock().ok()?.get(&oa)?.tools.get(tid).cloned()?;
            k.chat_item(&oa, &item).and_then(|i| i["title"].as_str().map(String::from))
        })
        .unwrap_or_else(|| "The agent asks for permission".into());
    let options: Vec<Value> = params
        .get("options")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|o| json!({ "optionId": o["optionId"], "name": o["name"], "kind": o["kind"] }))
        .collect();
    let (item_id, rx) = crate::consent::open(k, &oa, json!({ "title": title, "text": title, "options": options }), false);
    {
        let mut turns = c.turns.locked();
        let t = turns.entry(oa.clone()).or_default();
        t.last_item = Some(item_id.clone());
        t.last_kind = "permission".into();
    }
    k.research.log("agent", Kind::Permission, json!({ "phase": "shown", "session": oa, "id": item_id, "title": title, "options": options }));
    let k = k.clone();
    let c = c.clone();
    let started = Instant::now();
    tokio::spawn(async move {
        let choice = rx.await.unwrap_or(None);
        let outcome = match &choice {
            Some(opt) => json!({ "outcome": { "outcome": "selected", "optionId": opt } }),
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        c.rpc.respond(id, outcome);
        crate::consent::close(&k, &oa, &item_id, &choice);
        k.research.log(
            "user",
            Kind::Permission,
            json!({ "phase": "decided", "id": item_id, "option": choice, "ms": started.elapsed().as_millis() as u64 }),
        );
    });
}

/// ACP elicitation (`requestedSchema` properties) → a question item (§12.4).
fn handle_elicitation(k: &Arc<Kernel>, c: &Arc<AcpConn>, id: Value, params: &Value) {
    let oa = params
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(|a| c.oa_session(a))
        .unwrap_or_else(|| k.active_session());
    let message = params.get("message").and_then(Value::as_str).unwrap_or("The agent has a question").to_string();
    let props = params.pointer("/requestedSchema/properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut questions = Vec::new();
    let mut kinds = HashMap::new();
    for (key, schema) in &props {
        let options: Option<Vec<Value>> = schema
            .get("enum")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(|s| json!({ "label": s, "description": "" })).collect())
            .or_else(|| (schema["type"] == "boolean").then(|| vec![json!({ "label": "true", "description": "" }), json!({ "label": "false", "description": "" })]));
        kinds.insert(key.clone(), schema["type"].as_str().unwrap_or("string").to_string());
        questions.push(json!({
            "id": key,
            "header": schema.get("title").and_then(Value::as_str).unwrap_or(key),
            "question": schema.get("description").and_then(Value::as_str).unwrap_or(&message),
            "options": options,
            "isOther": options_is_none(schema),
            "isSecret": false,
        }));
    }
    if questions.is_empty() {
        questions.push(json!({ "id": "answer", "header": "Question", "question": message, "options": null, "isOther": true, "isSecret": false }));
        kinds.insert("answer".into(), "string".into());
    }
    let (_, rx) = k.ask_questions(&oa, questions, "acp");
    let c = c.clone();
    tokio::spawn(async move {
        match rx.await.unwrap_or(None) {
            Some(answers) => {
                let mut content = Map::new();
                for (key, v) in answers {
                    let first = v.as_array().and_then(|a| a.first()).cloned().unwrap_or(v);
                    let s = first.as_str().unwrap_or("").to_string();
                    let val = match kinds.get(&key).map(String::as_str) {
                        Some("boolean") => json!(s == "true"),
                        Some("number") | Some("integer") => s.parse::<f64>().map(|n| json!(n)).unwrap_or(json!(s)),
                        _ => json!(s),
                    };
                    content.insert(key, val);
                }
                c.rpc.respond(id, json!({ "action": "accept", "content": content }));
            }
            None => c.rpc.respond(id, json!({ "action": "cancel" })),
        }
    });
}

fn options_is_none(schema: &Value) -> bool {
    schema.get("enum").is_none() && schema["type"] != "boolean"
}

fn handle_update(k: &Arc<Kernel>, c: &Arc<AcpConn>, oa: &str, u: &Value) {
    let mut turns = c.turns.locked();
    let t = turns.entry(oa.to_string()).or_default();
    let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let Some(text) = u.pointer("/content/text").and_then(Value::as_str) else { return };
            let item_kind = if kind == "agent_message_chunk" { "agent" } else { "thought" };
            if item_kind == "agent" {
                t.text.push_str(text);
            }
            match (&t.last_item, t.last_kind == item_kind) {
                (Some(id), true) => {
                    let id = id.clone();
                    drop(turns);
                    k.chat_append_text_in(oa, &id, text);
                }
                _ => {
                    let id = k.chat_append_to(oa, json!({ "kind": item_kind, "text": text }));
                    t.last_item = Some(id);
                    t.last_kind = item_kind.into();
                }
            }
        }
        "tool_call" | "tool_call_update" => {
            let Some(tid) = u.get("toolCallId").and_then(Value::as_str) else { return };
            let title = u.get("title").and_then(Value::as_str);
            let status = u.get("status").and_then(Value::as_str);
            match t.tools.get(tid).cloned() {
                Some(item) => {
                    drop(turns);
                    let mut patch = Map::new();
                    if let Some(x) = title {
                        patch.insert("title".into(), json!(x));
                    }
                    if let Some(s) = status {
                        patch.insert("status".into(), json!(s));
                        if s == "completed" || s == "failed" {
                            k.research.log("agent", Kind::ToolCall, json!({ "id": tid, "status": s, "title": title }));
                        }
                    }
                    if !patch.is_empty() {
                        k.chat_update_in(oa, &item, patch);
                    }
                }
                None => {
                    let id = k.chat_append_to(oa, json!({
                        "kind": "tool",
                        "title": title.unwrap_or("Tool call"),
                        "status": status.unwrap_or("pending"),
                    }));
                    t.tools.insert(tid.to_string(), id.clone());
                    t.last_item = Some(id);
                    t.last_kind = "tool".into();
                    k.research.log(
                        "agent",
                        Kind::ToolCall,
                        json!({ "id": tid, "title": title, "kind": u.get("kind"), "status": status.unwrap_or("pending") }),
                    );
                }
            }
        }
        "plan" => {
            let entries = u.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut text = String::from("Plan:");
            for e in &entries {
                let mark = match e.get("status").and_then(Value::as_str) {
                    Some("completed") => "[x]",
                    Some("in_progress") => "[~]",
                    _ => "[ ]",
                };
                text.push_str(&format!("\n{mark} {}", e.get("content").and_then(Value::as_str).unwrap_or("")));
            }
            match t.plan_item.clone() {
                Some(id) => {
                    drop(turns);
                    let mut patch = Map::new();
                    patch.insert("text".into(), json!(text));
                    k.chat_update_in(oa, &id, patch);
                }
                None => {
                    let id = k.chat_append_to(oa, json!({ "kind": "system", "text": text }));
                    t.plan_item = Some(id.clone());
                    t.last_item = Some(id);
                    t.last_kind = "plan".into();
                }
            }
        }
        _ => {}
    }
}

/// One user turn in an OpenApp session: wait for the agent, ensure the ACP session,
/// session/prompt, stream updates into that session's chat, log the turn.
pub async fn run_turn(k: Arc<Kernel>, oa: String, text: String, build: bool) {
    let lock = k.turn_locks.locked().entry(oa.clone()).or_default().clone();
    let _turn = lock.lock().await;
    let _tg = crate::kernel::TurnGuard::begin(&k, &oa);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (status, err) = k.agent_status();
        match status.as_str() {
            "ready" | "working" => break,
            "starting" if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(250)).await,
            _ => {
                let msg = format!(
                    "The agent is not available ({status}){}",
                    err.map(|e| format!(": {e}")).unwrap_or_default()
                );
                k.system_message_in(&oa, "error", &msg);
                k.research.log("kernel", Kind::TurnEnd, json!({ "session": oa, "error": msg }));
                return;
            }
        }
    }
    let Some(c) = conn(&k) else {
        k.system_message_in(&oa, "error", "The agent is not connected.");
        return;
    };
    turn_started(&k);
    let acp = match ensure_session(&k, &c, &oa).await {
        Ok(a) => a,
        Err(e) => {
            k.system_message_in(&oa, "error", &e);
            k.research.log("kernel", Kind::TurnEnd, json!({ "session": oa, "error": e }));
            k.touch_session(&oa);
            turn_finished(&k);
            return;
        }
    };
    let mode = if build { "build" } else { "chat" };
    let mut text = text;
    let mut followups = 0;
    loop {
        let chosen = apply_choice(&k, &c, &oa, &acp, mode).await;
        c.turns.locked().insert(oa.clone(), Turn::default());
        let t0 = Instant::now();
        let res = c
            .rpc.request(
                "session/prompt",
                json!({ "sessionId": acp, "prompt": prompt_blocks(&k, &c, &oa, &text, build) }),
                Duration::from_secs(6 * 3600),
            )
            .await;
        let ms = t0.elapsed().as_millis() as u64;
        let full = c.turns.locked().remove(&oa).map(|t| t.text).unwrap_or_default();
        if !full.is_empty() {
            k.research.log("agent", Kind::AgentMessage, json!({ "session": oa, "text": full }));
        }
        // answer leftover permission prompts of this turn as cancelled
        cancel_open_permissions(&k, Some(&oa));
        let mut completed = false;
        match res {
            Ok(v) => {
                let stop = v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn").to_string();
                k.research.log(crate::research::turn_end_actor(&stop), Kind::TurnEnd, json!({ "session": oa, "stop_reason": stop, "ms": ms }));
                match stop.as_str() {
                    "end_turn" => completed = true,
                    "cancelled" => k.system_message_in(&oa, "system", "Stopped."),
                    other => k.system_message_in(&oa, "system", &format!("The agent stopped: {other}.")),
                }
            }
            Err(e) => {
                k.research.log("agent", Kind::TurnEnd, json!({ "session": oa, "error": e, "ms": ms }));
                k.system_message_in(&oa, "error", &format!("The agent request failed: {e}"));
                if let Some(msg) = chosen.as_ref().and_then(|r| crate::primitives::models::reject(&k, &c.harness_id, &r.model.id, &e)) {
                    k.system_message_in(&oa, "system", &msg);
                }
            }
        }
        // §23: an approved escalation continues the task on the strong model in one follow-up prompt
        match crate::primitives::models::take_followup(&k, &oa) {
            Some(reason) if completed && followups < 2 => {
                followups += 1;
                text = format!("[OpenApp] You now run on the stronger model ({reason}). Continue the user's last request from where you stopped.");
            }
            _ => break,
        }
    }
    crate::primitives::models::end_turn(&k, &oa);
    k.touch_session(&oa);
    turn_finished(&k);
}

/// Cancel the turn of the active session.
pub fn cancel(k: &Kernel) {
    let oa = k.active_session();
    if let Some(c) = conn(k)
        && let Some(acp) = c.acp_id(&oa) {
            c.rpc.notify("session/cancel", Some(json!({ "sessionId": acp })));
        }
}

/// Kill the harness process group (shutdown).
pub fn kill(k: &Kernel) {
    if let Some(c) = conn(k)
        && let Some(pid) = c.pid {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).status();
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
            }
        }
}
