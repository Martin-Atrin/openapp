//! G10 agent modules (contract §21).
//!
//! Manifest `"agents": [{ name, instructions: "AGENT.md", tools: ["<module>/<tool>" | "<module>/*" | "host:<method>" |
//! "host:<module>/<method>" | "openapp:<tool or host action>"], model?: "default" | "on-device",
//! budget?: { turns (default 8), minutes (default 5) }, triggers?: [job names] }]`.
//! - `agent.run { module, agent, input?, wait? }` → run id (or the result with `wait`); `agent.cancel { run }`.
//!   Jobs (G6) whose name is in `triggers` run the agent with the job input.
//! - Only the listed tools exist for the agent; every call goes through [`call_gated`] (anything else is refused and
//!   logged). Tools run as the agent's module (`module:<id>`: egress, secrets and consent cards apply as usual).
//! - `on-device`: a small loop on the Mac's model (`ai.generate` with a guided JSON schema whose tool name is an enum of
//!   the allowed tools; one action per turn). `default`: Pi SDK in the app VM (workspace-template/pi/agent-runner.ts)
//!   with the VM's Pi sign-in; tools proxy through `POST $OPENAPP_HOST_URL/agent.tool` with a per-run token. When Pi
//!   is not installed or not signed in, a `default` agent runs on-device (noted in the run).
//! - Runs are visible: host data `agents` (declared agents + last run) and `agent_runs`, channel `kernel/agents`
//!   ({op: start|step|result|error|cancelled, run, agent}), research log (tool names and status, no content).

use crate::research::Kind;
use crate::util::{MutexExt, truncate, valid_ident};
use super::channels::{self, ChannelData};
use super::*;
use crate::modules::safe_rel;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const RUNNER_TS: &str = include_str!("../../../workspace-template/pi/agent-runner.ts");
const MAX_RUNS: usize = 30;
const RESULT_CHARS: usize = 1500;

#[derive(Clone, Debug)]
pub struct AgentSpec {
    pub module: String,
    pub name: String,
    pub instructions: String,
    pub tools: Vec<String>,
    pub model: String,
    pub turns: u32,
    pub minutes: u32,
    pub triggers: Vec<String>,
}

pub fn parse_agents(module: &str, raw: &Value) -> (Vec<AgentSpec>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let Some(list) = raw.get("agents") else { return (out, problems) };
    let Some(arr) = list.as_array() else {
        problems.push(format!("modules/{module}/openapp.json: \"agents\" must be an array"));
        return (out, problems);
    };
    for (i, a) in arr.iter().enumerate() {
        let p = |m: String| format!("modules/{module}/openapp.json agents[{i}]: {m}");
        let Some(name) = a["name"].as_str().filter(|n| valid_ident(n, 40)) else {
            problems.push(p("\"name\" must match [A-Za-z0-9_-]{1,40}".into()));
            continue;
        };
        let instructions = a["instructions"].as_str().unwrap_or("AGENT.md").to_string();
        let tools: Vec<String> = a["tools"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
        if let Some(bad) = tools.iter().find(|t| !(t.starts_with("host:") || t.starts_with("openapp:") || t.split_once('/').is_some_and(|(m, n)| crate::modules::valid_module_id(m) && !n.is_empty()))) {
            problems.push(p(format!("tool \"{bad}\" must be \"<module>/<tool>\", \"host:<method>\" or \"openapp:<tool>\"")));
            continue;
        }
        let model = a["model"].as_str().unwrap_or("default").to_string();
        if model != "default" && model != "on-device" {
            problems.push(p(format!("\"model\" must be \"default\" or \"on-device\", got \"{model}\"")));
            continue;
        }
        let b = &a["budget"];
        out.push(AgentSpec {
            module: module.into(),
            name: name.into(),
            instructions,
            tools,
            model,
            turns: b["turns"].as_u64().unwrap_or(8).clamp(1, 100) as u32,
            minutes: b["minutes"].as_u64().unwrap_or(5).clamp(1, 120) as u32,
            triggers: a["triggers"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default(),
        });
    }
    (out, problems)
}

fn all_agents(k: &Kernel) -> Vec<AgentSpec> {
    let mods: Vec<(String, Value)> = k.lock().modules.values().filter(|m| m.status == "running" || m.status == "idle").filter_map(|m| m.manifest.as_ref().map(|mf| (m.id.clone(), mf.raw.clone()))).collect();
    mods.iter().flat_map(|(id, raw)| parse_agents(id, raw).0).collect()
}

fn find(k: &Kernel, module: &str, name: &str) -> Result<AgentSpec, String> {
    let raw = k.lock().modules.get(module).and_then(|m| m.manifest.as_ref().map(|mf| mf.raw.clone())).ok_or_else(|| format!("unknown module \"{module}\""))?;
    let (list, problems) = parse_agents(module, &raw);
    list.into_iter().find(|a| a.name == name).ok_or_else(|| if problems.is_empty() { format!("module {module} has no agent \"{name}\"") } else { problems.join("; ") })
}

/// Agents of `module` that list job `job` in `triggers`.
pub fn triggered_by(k: &Kernel, module: &str, job: &str) -> Vec<String> {
    let full = format!("{module}/{job}");
    all_agents(k).into_iter().filter(|a| a.module == module && a.triggers.iter().any(|t| t == job || *t == full)).map(|a| a.name).collect()
}

// ---------- runs ----------

pub struct RunCtx {
    pub module: String,
    pub agent: String,
    /// allowed tool targets (expanded)
    pub allowed: Vec<String>,
}

#[derive(Default)]
struct Agents {
    runs: Mutex<VecDeque<Value>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    ctx: Mutex<HashMap<String, Arc<RunCtx>>>,
}

fn st(k: &Kernel) -> Arc<Agents> {
    k.prims.get::<Agents>()
}

fn patch_run(k: &Kernel, run: &str, f: impl FnOnce(&mut Map<String, Value>)) {
    let s = st(k);
    let mut runs = s.runs.locked();
    if let Some(r) = runs.iter_mut().find(|r| r["run"] == run).and_then(Value::as_object_mut) {
        f(r);
    }
}

fn publish(k: &Kernel, v: Value) {
    channels::publish(k, "kernel/agents", ChannelData::Json(v));
}

/// Model-facing name of a tool target (`mail/list` → `mail__list`, `host:ai.generate` → `host_ai_generate`).
pub fn tool_name(target: &str) -> String {
    target.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>().replacen('_', "__", if target.contains('/') && !target.contains(':') { 1 } else { 0 })
}

#[derive(Clone, Debug)]
pub struct ToolDesc {
    pub name: String,
    pub target: String,
    pub description: String,
    pub schema: Value,
}

/// Expand and describe the allowed tools (activates the modules whose tools are listed).
pub async fn resolve_tools(k: &Arc<Kernel>, patterns: &[String]) -> Result<Vec<ToolDesc>, String> {
    let mut out: Vec<ToolDesc> = Vec::new();
    let mut push = |t: ToolDesc| {
        if !out.iter().any(|x| x.target == t.target) {
            out.push(t);
        }
    };
    for p in patterns {
        if let Some(m) = p.strip_prefix("host:") {
            let methods = k.host_services_info()["methods"].clone();
            let desc = methods.get(m).and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("Mac host method {m}"));
            push(ToolDesc { name: tool_name(p), target: p.clone(), description: desc, schema: json!({ "type": "object" }) });
        } else if let Some(t) = p.strip_prefix("openapp:") {
            let mut tools = crate::mcp_server::tools();
            tools.extend(super::mcp_tools());
            if let Some(tl) = tools.iter().find(|x| x.name == t) {
                push(ToolDesc { name: tool_name(p), target: p.clone(), description: tl.description.as_deref().unwrap_or("").to_string(), schema: Value::Object((*tl.input_schema).clone()) });
            } else if let Some(a) = super::action(t) {
                push(ToolDesc { name: tool_name(p), target: p.clone(), description: format!("OpenApp host action {t} {}", a.args), schema: json!({ "type": "object" }) });
            } else {
                return Err(format!("agent tool \"{p}\": no openapp tool or host action \"{t}\""));
            }
        } else if let Some((m, t)) = p.split_once('/') {
            super::lifecycle::ensure_active(k, m).await.map_err(|e| format!("agent tool \"{p}\": {e}"))?;
            let tools = k.lock().modules.get(m).map(|e| e.tools.clone()).ok_or_else(|| format!("agent tool \"{p}\": unknown module \"{m}\""))?;
            let mut found = false;
            for tl in tools.iter().filter(|x| t == "*" || x.name == t) {
                let target = format!("{m}/{}", tl.name);
                push(ToolDesc { name: tool_name(&target), target, description: tl.description.as_deref().unwrap_or("").to_string(), schema: Value::Object((*tl.input_schema).clone()) });
                found = true;
            }
            if !found {
                return Err(format!("agent tool \"{p}\": module {m} has no tool \"{t}\""));
            }
        }
    }
    Ok(out)
}

fn call_result_json(r: &rmcp::model::CallToolResult) -> Value {
    json!({ "text": crate::modules::result_text(r), "structured": r.structured_content, "isError": r.is_error.unwrap_or(false) })
}

/// The only way an agent run reaches a tool: refused unless `target` is one of the run's allowed tools.
pub async fn call_gated(k: &Arc<Kernel>, ctx: &RunCtx, run: &str, target: &str, args: Value) -> Result<Value, String> {
    let actor = format!("agent:{}/{}", ctx.module, ctx.agent);
    if !ctx.allowed.iter().any(|a| a == target) {
        k.research.log(&actor, Kind::AgentTool, json!({ "run": run, "tool": target, "refused": true }));
        return Err(format!("refused: \"{target}\" is not one of this agent's tools ({})", ctx.allowed.join(", ")));
    }
    let args = if args.is_object() { args } else { json!({}) };
    let as_module = format!("module:{}", ctx.module);
    let t0 = Instant::now();
    let r = if let Some(m) = target.strip_prefix("host:") {
        let (module, method) = match m.split_once('/') {
            Some((a, b)) => (Some(a), b),
            None => (None, m),
        };
        k.host_call(module, method, args, &as_module).await.map_err(String::from)
    } else if let Some(t) = target.strip_prefix("openapp:") {
        let mut tools = crate::mcp_server::tools();
        tools.extend(super::mcp_tools());
        if tools.iter().any(|x| x.name == t) {
            let m = crate::mcp_server::KernelMcp { k: k.clone() };
            let r = m.dispatch(t, args.as_object().cloned().unwrap_or_default()).await;
            if r.is_error == Some(true) { Err(call_result_json(&r).to_string()) } else { Ok(call_result_json(&r)) }
        } else {
            call_action(k, t, args, Caller::new(as_module.clone())).await.unwrap_or_else(|| Err(format!("unknown host action \"{t}\"")))
        }
    } else if let Some((m, t)) = target.split_once('/') {
        match k.call_module_tool(m, t, args.as_object().cloned().unwrap_or_default()).await {
            Ok(r) => {
                k.after_module_call(m, &r, false);
                if r.is_error == Some(true) { Err(crate::modules::result_text(&r)) } else { Ok(call_result_json(&r)) }
            }
            Err(e) => Err(e),
        }
    } else {
        Err(format!("bad tool \"{target}\""))
    };
    k.research.log(&actor, Kind::AgentTool, json!({ "run": run, "tool": target, "ok": r.is_ok(), "ms": t0.elapsed().as_millis() as u64 }));
    r
}

fn read_instructions(k: &Kernel, spec: &AgentSpec) -> Result<String, String> {
    let i = &spec.instructions;
    if i.contains('\n') || (i.contains(' ') && !i.ends_with(".md")) {
        return Ok(i.clone());
    }
    if !safe_rel(i) {
        return Err(format!("agent {}: instructions \"{i}\" must be a file inside the module", spec.name));
    }
    let p = k.workspace.join("modules").join(&spec.module).join(i);
    let t = std::fs::read_to_string(&p).map_err(|_| format!("agent {}: modules/{}/{i} not found", spec.name, spec.module))?;
    Ok(t.chars().take(32_000).collect())
}

fn step(k: &Kernel, run: &str, agent: &str, v: Value) {
    patch_run(k, run, |r| {
        if let Some(a) = r.get_mut("steps").and_then(Value::as_array_mut) {
            a.push(v.clone());
        }
    });
    let mut ev = json!({ "op": "step", "run": run, "agent": agent });
    if let (Some(o), Some(src)) = (ev.as_object_mut(), v.as_object()) {
        for (a, b) in src {
            o.insert(a.clone(), b.clone());
        }
    }
    publish(k, ev);
    k.emit_snapshot();
}

// ---------- on-device loop ----------

#[allow(clippy::too_many_arguments)]
async fn on_device(k: &Arc<Kernel>, ctx: &RunCtx, run: &str, spec: &AgentSpec, instructions: &str, tools: &[ToolDesc], input: &str, cancel: &CancellationToken) -> Result<(String, u32), String> {
    let agent = format!("{}/{}", spec.module, spec.name);
    let mut tool_doc = String::new();
    for t in tools {
        let props = t.schema.get("properties").map(|p| truncate(&p.to_string(), 300)).unwrap_or_else(|| "{}".into());
        tool_doc.push_str(&format!("- {}: {} (arguments: {props})\n", t.name, truncate(&t.description, 200)));
    }
    let names: Vec<Value> = tools.iter().map(|t| json!(t.name)).collect();
    let mut props = json!({
        "action": { "type": "string", "enum": ["tool", "final"], "description": "tool = call one tool now; final = the task is done" },
        "args_json": { "type": "string", "description": "the tool arguments as a JSON object, e.g. {\"a\": 1}" },
        "text": { "type": "string", "description": "for final: the complete answer for the user (never empty)" },
    });
    if !names.is_empty() {
        props["tool"] = json!({ "type": "string", "enum": names, "description": "the tool to call" });
    }
    let schema = json!({ "type": "object", "properties": props, "required": ["action"] });
    let sys = format!(
        "{instructions}\n\nYou work step by step. Each reply is exactly one action: call one tool, or give the final answer.\nTools:\n{}Use a tool when you need its result; after you have what you need, answer with action \"final\".",
        if tool_doc.is_empty() { "(none)\n".to_string() } else { tool_doc }
    );
    let mut history = String::new();
    let actor = format!("agent:{agent}");
    let mut last = String::new();
    for turn in 1..=spec.turns {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let prompt = format!("Task: {input}\n\n{}What is your next action?", if history.is_empty() { String::new() } else { format!("Done so far:\n{history}\n") });
        let r = tokio::select! {
            r = k.host_call(None, "ai.generate", json!({ "prompt": prompt, "instructions": sys, "schema": schema, "temperature": 0 }), &actor) => r?,
            _ = cancel.cancelled() => return Err("cancelled".into()),
        };
        let a = r.get("json").cloned().or_else(|| r["text"].as_str().and_then(|t| serde_json::from_str(t).ok())).ok_or("the on-device model returned no JSON action")?;
        match a["action"].as_str() {
            Some("final") => {
                // a terse model sometimes ends without text: the last tool result is the answer then
                let t = a["text"].as_str().unwrap_or("").trim().to_string();
                return Ok((if t.is_empty() { last.clone() } else { t }, turn));
            }
            Some("tool") => {
                let name = a["tool"].as_str().unwrap_or("");
                let target = tools.iter().find(|t| t.name == name || t.target == name).map(|t| t.target.clone()).unwrap_or_else(|| name.to_string());
                let args: Value = a["args_json"].as_str().and_then(|s| serde_json::from_str(s).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
                let t0 = Instant::now();
                let res = call_gated(k, ctx, run, &target, args.clone()).await;
                let (ok, text) = match &res {
                    Ok(v) => (true, v["text"].as_str().map(String::from).unwrap_or_else(|| v.to_string())),
                    Err(e) => (false, format!("error: {e}")),
                };
                step(k, run, &agent, json!({ "turn": turn, "tool": target, "ok": ok, "ms": t0.elapsed().as_millis() as u64, "summary": truncate(&text, 160) }));
                history.push_str(&format!("{turn}. {name} {} → {}\n", args, truncate(&text, RESULT_CHARS)));
                if ok {
                    last = truncate(&text, 4000);
                }
            }
            _ => return Err(format!("the on-device model returned an unknown action: {}", truncate(&a.to_string(), 200))),
        }
    }
    Err(format!("budget: {} turns used without a final answer", spec.turns))
}

// ---------- Pi in the VM ----------

async fn pi_ready(k: &Kernel) -> Result<Arc<crate::vm::AppVm>, String> {
    let vm = k.vm().ok_or("the app VM is not running")?;
    let (code, _, _) = vm.sh(&format!("test -f {}/dist/index.js && test -s {}/auth.json", crate::pi::pkg_dir(), crate::pi::VM_PI_HOME), &[]).await?;
    if code != 0 {
        return Err("Pi is not installed or not signed in in the app VM".into());
    }
    Ok(vm)
}

#[allow(clippy::too_many_arguments)]
async fn pi_run(k: &Arc<Kernel>, vm: &crate::vm::AppVm, run: &str, spec: &AgentSpec, instructions: &str, tools: &[ToolDesc], input: &str, cancel: &CancellationToken) -> Result<(String, u32), String> {
    let agent = format!("{}/{}", spec.module, spec.name);
    let runner = "/run/openapp/agent-runner.ts";
    vm.write_file(runner, RUNNER_TS.as_bytes()).await?;
    let job_path = format!("/run/openapp/agents/{run}.json");
    let job = json!({
        "run": run, "instructions": instructions, "input": input, "turns": spec.turns,
        "cwd": format!("{}/modules/{}", crate::vm::GUEST_WS, spec.module), "agentDir": crate::pi::VM_PI_HOME,
        "tools": tools.iter().map(|t| json!({ "name": t.name, "target": t.target, "description": t.description, "inputSchema": t.schema })).collect::<Vec<_>>(),
    });
    vm.write_file(&job_path, job.to_string().as_bytes()).await?;
    let mut env = crate::pi::guest_env(None);
    env.push(("OPENAPP_PI_PKG".into(), crate::pi::pkg_dir()));
    env.push(("OPENAPP_HOST_URL".into(), format!("http://127.0.0.1:{}/host", crate::vm::MCP_PORT)));
    env.push(("OPENAPP_AGENT_TOKEN".into(), k.host_token(&format!("agentrun:{run}"))));
    let mut child = vm.spawn("node", &[runner.to_string(), job_path.clone()], &env, Some(&format!("{}/modules/{}", crate::vm::GUEST_WS, spec.module))).await?;
    let killer = child.killer();
    if let Some(se) = child.stderr.take() {
        let log = k.workspace.join(".openapp").join("logs").join(format!("{}.agent.log", spec.module));
        crate::modules::pump_stderr(se, log, Arc::new(Mutex::new(VecDeque::new())));
    }
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
    let mut result: Option<Result<(String, u32), String>> = None;
    let mut n = 0u32;
    loop {
        let line = tokio::select! {
            l = lines.next_line() => l,
            _ = cancel.cancelled() => {
                killer.kill();
                result = Some(Err("cancelled".into()));
                break;
            }
        };
        let Ok(Some(line)) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("step") => {
                n += 1;
                step(k, run, &agent, json!({ "turn": n, "tool": v["tool"], "ok": v["ok"] }));
            }
            Some("result") => result = Some(Ok((v["text"].as_str().unwrap_or("").to_string(), v["turns"].as_u64().unwrap_or(0) as u32))),
            Some("error") => result = Some(Err(v["error"].as_str().unwrap_or("error").to_string())),
            Some("budget") => step(k, run, &agent, json!({ "budget": "turns", "turns": v["turns"] })),
            _ => {}
        }
    }
    let _ = vm.sh(&format!("rm -f '{job_path}'"), &[]).await;
    result.unwrap_or_else(|| Err("the Pi runner exited without a result (see .openapp/logs/<module>.agent.log)".into()))
}

// ---------- entry points ----------

async fn execute(k: &Arc<Kernel>, run: &str, spec: &AgentSpec, input: &str, cancel: &CancellationToken) -> Result<(String, u32, String), String> {
    let instructions = read_instructions(k, spec)?;
    let tools = resolve_tools(k, &spec.tools).await?;
    let ctx = Arc::new(RunCtx { module: spec.module.clone(), agent: spec.name.clone(), allowed: tools.iter().map(|t| t.target.clone()).collect() });
    st(k).ctx.locked().insert(run.to_string(), ctx.clone());
    patch_run(k, run, |r| {
        r.insert("tools".into(), json!(ctx.allowed));
    });
    let res = if spec.model == "default" {
        match pi_ready(k).await {
            Ok(vm) => pi_run(k, &vm, run, spec, &instructions, &tools, input, cancel).await.map(|(t, n)| (t, n, "pi".to_string())),
            Err(why) => {
                step(k, run, &format!("{}/{}", spec.module, spec.name), json!({ "note": format!("{why}: running on the on-device model") }));
                on_device(k, &ctx, run, spec, &instructions, &tools, input, cancel).await.map(|(t, n)| (t, n, "on-device".to_string()))
            }
        }
    } else {
        on_device(k, &ctx, run, spec, &instructions, &tools, input, cancel).await.map(|(t, n)| (t, n, "on-device".to_string()))
    };
    st(k).ctx.locked().remove(run);
    res
}

/// Start a run and wait for its result `{ run, status, text, turns, model }` (Err on failure / cancel / budget).
pub async fn run_wait(k: &Arc<Kernel>, module: &str, agent: &str, input: Value, trigger: &str, extra: Value) -> Result<Value, String> {
    let spec = find(k, module, agent)?;
    let run = format!("ag-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    let id = format!("{module}/{agent}");
    let input_text = match &input {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let cancel = CancellationToken::new();
    st(k).cancels.locked().insert(run.clone(), cancel.clone());
    {
        let s = st(k);
        let mut runs = s.runs.locked();
        runs.push_back(json!({ "run": run, "agent": id, "module": module, "name": agent, "status": "running", "trigger": trigger, "model": spec.model, "started": crate::state::now_ts(), "steps": [], "context": extra }));
        while runs.len() > MAX_RUNS {
            runs.pop_front();
        }
    }
    publish(k, json!({ "op": "start", "run": run, "agent": id, "trigger": trigger, "model": spec.model }));
    k.research.log(&format!("agent:{id}"), Kind::AgentRun, json!({ "phase": "start", "run": run, "trigger": trigger, "model": spec.model }));
    k.emit_snapshot();
    let t0 = Instant::now();
    let budget = Duration::from_secs(spec.minutes as u64 * 60);
    let res = match tokio::time::timeout(budget, execute(k, &run, &spec, &input_text, &cancel)).await {
        Ok(r) => r,
        Err(_) => {
            cancel.cancel();
            Err(format!("budget: {} minutes used", spec.minutes))
        }
    };
    st(k).cancels.locked().remove(&run);
    let ms = t0.elapsed().as_millis() as u64;
    let (status, op) = match &res {
        Ok(_) => ("ok", "result"),
        Err(e) if e == "cancelled" => ("cancelled", "cancelled"),
        Err(e) if e.starts_with("budget") => ("budget", "error"),
        Err(_) => ("error", "error"),
    };
    let (text, turns, model) = res.as_ref().map(|(t, n, m)| (t.clone(), *n, m.clone())).unwrap_or_default();
    let err = res.as_ref().err().cloned();
    patch_run(k, &run, |r| {
        r.insert("status".into(), json!(status));
        r.insert("ms".into(), json!(ms));
        r.insert("text".into(), json!(text));
        r.insert("turns".into(), json!(turns));
        r.insert("error".into(), json!(err));
        if !model.is_empty() {
            r.insert("model_used".into(), json!(model));
        }
        r.insert("ended".into(), json!(crate::state::now_ts()));
    });
    publish(k, json!({ "op": op, "run": run, "agent": id, "status": status, "text": truncate(&text, 4000), "error": err, "ms": ms, "turns": turns }));
    k.research.log(&format!("agent:{id}"), Kind::AgentRun, json!({ "phase": "end", "run": run, "status": status, "ms": ms, "turns": turns, "model": model }));
    k.emit_snapshot();
    match res {
        Ok(_) => Ok(json!({ "ok": true, "run": run, "status": status, "text": text, "turns": turns, "model": model, "ms": ms })),
        Err(e) => Err(format!("agent {id} ({run}): {e}")),
    }
}

fn module_arg(k: &Kernel, p: &Value, c: &Caller) -> Result<String, String> {
    let m = match (c.module(), arg(p, "module")) {
        (Some(own), Some(m)) if own != m => return Err(format!("{} may only run its own agents (not \"{m}\")", c.id)),
        (Some(own), _) => own.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => return Err("\"module\" is required".into()),
    };
    if !k.lock().modules.contains_key(&m) {
        return Err(format!("unknown module \"{m}\""));
    }
    Ok(m)
}

async fn act_run(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let module = module_arg(&k, &p, &c)?;
    let agent = arg(&p, "agent").or_else(|| arg(&p, "name")).ok_or("agent.run needs \"agent\"")?.to_string();
    find(&k, &module, &agent)?;
    let input = p.get("input").cloned().unwrap_or(Value::Null);
    let trigger = if c.id == "user" { "user".to_string() } else { c.id.clone() };
    if p.get("wait").and_then(Value::as_bool) == Some(true) {
        return run_wait(&k, &module, &agent, input, &trigger, Value::Null).await;
    }
    let before: Vec<String> = st(&k).runs.locked().iter().filter_map(|r| r["run"].as_str().map(String::from)).collect();
    let k2 = k.clone();
    let (m2, a2) = (module.clone(), agent.clone());
    k.spawn(async move {
        let _ = run_wait(&k2, &m2, &a2, input, &trigger, Value::Null).await;
    });
    // the run id appears as soon as the run is registered
    for _ in 0..100 {
        let id = st(&k).runs.locked().iter().rev().find(|r| r["agent"] == format!("{module}/{agent}").as_str() && r["run"].as_str().is_some_and(|x| !before.iter().any(|b| b == x))).and_then(|r| r["run"].as_str().map(String::from));
        if let Some(id) = id {
            return Ok(json!({ "ok": true, "run": id, "note": "follow it on kernel/agents or host data agent_runs; agent.cancel { run } stops it" }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(json!({ "ok": true, "note": "started" }))
}

async fn act_cancel(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let run = arg(&p, "run").ok_or("agent.cancel needs \"run\"")?;
    if let Some(m) = c.module() {
        let owner = st(&k).runs.locked().iter().find(|r| r["run"] == run).and_then(|r| r["module"].as_str().map(String::from));
        if owner.as_deref() != Some(m) {
            return Err(format!("{} may only cancel its own agent runs", c.id));
        }
    }
    let t = st(&k).cancels.locked().get(run).cloned().ok_or_else(|| format!("no running agent run \"{run}\""))?;
    t.cancel();
    k.research.log(&c.id, Kind::AgentRun, json!({ "phase": "cancel", "run": run }));
    Ok(json!({ "ok": true, "cancelled": run }))
}

/// `POST /host/agent.tool { run, tool, args }` from the Pi runner (token `agentrun:<run>`).
async fn ep_tool(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let run = arg(&p, "run").ok_or("\"run\" is required")?;
    if c.id != format!("agentrun:{run}") {
        return Err("agent.tool is only for the agent runner of this run (not allowed)".into());
    }
    let ctx = st(&k).ctx.locked().get(run).cloned().ok_or("this agent run is over")?;
    let tool = arg(&p, "tool").ok_or("\"tool\" is required")?;
    call_gated(&k, &ctx, run, tool, p.get("args").cloned().unwrap_or(json!({}))).await
}

fn agents_items(k: &Kernel) -> Value {
    let runs: Vec<Value> = st(k).runs.locked().iter().cloned().collect();
    Value::Array(
        all_agents(k)
            .iter()
            .map(|a| {
                let id = format!("{}/{}", a.module, a.name);
                let last = runs.iter().rev().find(|r| r["agent"] == id.as_str());
                let last_s = last.map(|r| format!(" · last {}", r["status"].as_str().unwrap_or("?"))).unwrap_or_default();
                let trig = if a.triggers.is_empty() { String::new() } else { format!(" · on {}", a.triggers.join(", ")) };
                json!({ "id": id, "title": a.name, "subtitle": format!("{} · {} · {} tools{trig}{last_s}", a.module, a.model, a.tools.len()), "icon": "bot",
                    "module": a.module, "name": a.name, "model": a.model, "tools": a.tools, "triggers": a.triggers, "budget": { "turns": a.turns, "minutes": a.minutes },
                    "status": last.map(|r| r["status"].clone()).unwrap_or(json!("never ran")), "last_run": last.map(|r| r["run"].clone()),
                    "actions": [{ "label": "Run", "action": { "host": "agent.run", "module": a.module, "agent": a.name } }] })
            })
            .collect(),
    )
}

fn runs_items(k: &Kernel) -> Value {
    Value::Array(
        st(k).runs.locked().iter().rev().map(|r| {
            let steps = r["steps"].as_array().map(|a| a.len()).unwrap_or(0);
            let mut v = r.clone();
            v["id"] = r["run"].clone();
            v["title"] = r["agent"].clone();
            v["subtitle"] = json!(format!("{} · {steps} steps · {}", r["status"].as_str().unwrap_or(""), r["trigger"].as_str().unwrap_or("")));
            v["icon"] = json!("bot");
            if r["status"] == "running" {
                v["actions"] = json!([{ "label": "Cancel", "action": { "host": "agent.cancel", "run": r["run"] } }]);
            }
            v
        }).collect(),
    )
}

pub fn primitive() -> Primitive {
    let run = handler(act_run);
    let cancel = handler(act_cancel);
    Primitive {
        name: "agents",
        purpose: "Agent modules: scoped agents declared in openapp.json (instructions, only the listed tools, budget, job triggers) run with Pi in the VM or the Mac's on-device model; runs are visible and cancellable.",
        example: r#"openapp.json: "agents": [{ "name": "summary", "instructions": "AGENT.md", "tools": ["mail/list_unread", "today/events", "openapp:notify.post"], "model": "default", "budget": { "turns": 12, "minutes": 5 }, "triggers": ["morning"] }], "jobs": [{ "name": "morning", "schedule": "0 8 * * *" }] · { "host": "agent.run", "module": "brief", "agent": "summary" }"#,
        class: Class::Free,
        actions: vec![
            Action { name: "agent.run", args: "{ module, agent, input?, wait? } → { run } (wait: { status, text, turns, model })", handler: run.clone() },
            Action { name: "agent.cancel", args: "{ run }", handler: cancel.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "agent.run", args: "{ agent, input?, wait? } (the calling module's agent)", handler: run },
            Endpoint { method: "agent.cancel", args: "{ run }", handler: cancel },
            Endpoint { method: "agent.tool", args: "(internal: the Pi agent runner's tool proxy)", handler: handler(ep_tool) },
        ],
        data: vec![
            DataSource { name: "agents", doc: "list items [{ id: \"<module>/<name>\", title, subtitle, icon: \"bot\", model, tools, triggers, status (last run), actions: [Run] }] — declared agents", get: data_fn(agents_items) },
            DataSource { name: "agent_runs", doc: "list items [{ id: run, title: agent, subtitle: \"status · N steps · trigger\", status: running|ok|error|budget|cancelled, steps[{turn, tool, ok, summary}], text, error }] — recent agent runs (newest first)", get: data_fn(runs_items) },
        ],
        describe_keys: vec![(
            "agents",
            data_fn(|k| {
                let mods: Vec<(String, Value)> = k.lock().modules.values().filter_map(|m| m.manifest.as_ref().map(|mf| (m.id.clone(), mf.raw.clone()))).collect();
                let problems: Vec<String> = mods.iter().flat_map(|(id, raw)| parse_agents(id, raw).1).collect();
                json!({ "agents": agents_items(k), "problems": problems, "recent_runs": st(k).runs.locked().iter().rev().take(5).map(|r| json!({ "run": r["run"], "agent": r["agent"], "status": r["status"], "error": r["error"] })).collect::<Vec<_>>() })
            }),
        )],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_names() {
        let raw = json!({ "agents": [
            { "name": "summary", "tools": ["mail/list", "host:ai.generate", "openapp:notify.post"], "budget": { "turns": 3 }, "triggers": ["morning"] },
            { "name": "bad", "tools": ["rm -rf"] },
            { "name": "x", "model": "gpt" }
        ] });
        let (a, p) = parse_agents("brief", &raw);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].turns, a[0].minutes, a[0].model.as_str(), a[0].instructions.as_str()), (3, 5, "default", "AGENT.md"));
        assert_eq!(p.len(), 2);
        assert_eq!(tool_name("mail/list"), "mail__list");
        assert_eq!(tool_name("host:ai.generate"), "host_ai_generate");
        assert_eq!(tool_name("openapp:notify.post"), "openapp_notify_post");
    }
}
