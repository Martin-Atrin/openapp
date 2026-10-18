//! The `openapp` MCP server: construction tools for the agent (contract §5).
//! Streamable HTTP on 127.0.0.1:<random>/mcp, bearer token required.

use crate::research::Kind;
use crate::kernel::{Kernel, McpEndpoint};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, InitializeRequestParams, DiscoverResult,
    InitializeResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const INSTRUCTIONS: &str = "You are running inside OpenApp. The user sees only what you build. Read AGENTS.md in the workspace. Call openapp_describe first.";

#[derive(Clone)]
pub struct KernelMcp {
    pub k: Arc<Kernel>,
}

fn schema(v: Value) -> Arc<Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(Map::new()),
    }
}

pub fn tools() -> Vec<Tool> {
    let mut v = vec![
        Tool::new(
            "openapp_describe",
            "Call first. Returns the contract version, workspace path, layout, theme, modules (status, views, tools), anchors in use and available, the spec view catalog, and host facts (OS, CPU, RAM, which runtimes/CLIs are on PATH).",
            schema(json!({ "type": "object", "properties": {} })),
        ),
        Tool::new(
            "openapp_module_load",
            "Validate modules/<id>/openapp.json, (re)start its MCP server, list its tools, parse its views, and apply its default placements on the first load. Call after you write or change a module. On failure returns a structured error with the last 40 stderr lines; fix and load again.",
            schema(json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "module id = folder name under modules/ ([a-z0-9-]+)" } },
                "required": ["id"]
            })),
        ),
        Tool::new(
            "openapp_module_unload",
            "Stop a module's server (status becomes stopped). Optionally remove its placements from the layout.",
            schema(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "remove_placements": { "type": "boolean", "default": false }
                },
                "required": ["id"]
            })),
        ),
        Tool::new(
            "openapp_layout",
            "Change the layout atomically (all ops validated first, then applied, saved and shown). Ops: {op:\"region_add\", id, edge:\"left\"|\"right\"|\"top\"|\"bottom\", size:px, mode?:\"stack\"|\"tabs\", title?} · {op:\"region_update\", id, size?, mode?, title?} · {op:\"region_remove\", id} (removes its placements) · {op:\"place\", module, view, anchor, label?, order?} (returns placement id p_xxxxxxxx) · {op:\"move\", placement, anchor, order?} · {op:\"unplace\", placement}. Anchors: composer.actions, composer.above, main, region:<id>, slot:<id>.",
            schema(json!({
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "op": { "type": "string", "enum": ["region_add", "region_update", "region_remove", "place", "move", "unplace"] } }, "required": ["op"] }
                    }
                },
                "required": ["ops"]
            })),
        ),
        Tool::new(
            "openapp_theme",
            "Merge theme tokens and validate them. Keys: bg, surface, surface-2, text, text-muted, accent, accent-text, border, success, warning, danger (CSS colors: #rgb, #rrggbb, rgb(), hsl(), oklch()), radius (px number), font, font-mono (font-family strings), density (compact|normal|comfy). null resets a token. Contrast of text on bg, text on surface and accent-text on accent must be >= 4.5:1, otherwise the change is rejected with the ratio.",
            schema(json!({
                "type": "object",
                "properties": { "tokens": { "type": "object", "additionalProperties": true } },
                "required": ["tokens"]
            })),
        ),
        Tool::new(
            "openapp_call",
            "Call a tool on a module's own MCP server (to test it). Returns the MCP CallToolResult. structuredContent.openapp directives (notify, composer_insert) are applied; prompt is skipped for test calls.",
            schema(json!({
                "type": "object",
                "properties": {
                    "module": { "type": "string" },
                    "tool": { "type": "string" },
                    "args": { "type": "object", "additionalProperties": true }
                },
                "required": ["module", "tool"]
            })),
        ),
        Tool::new(
            "openapp_snapshot",
            "A text tree of what the user sees now (regions, anchors, placements with their rendered spec views, html views), module status, view problems, and the last 20 UI errors (bad spec nodes, iframe errors, failed actions). Check this after every change.",
            schema(json!({ "type": "object", "properties": {} })),
        ),
    ];
    // §18 standard library tools: `host_<method with . → _>`
    v.extend(
        [
            (
                "speech.transcribe",
                "macOS on-device speech to text (SpeechAnalyzer, Neural Engine; no data leaves the Mac). path = workspace file (/workspace/... or relative; m4a/aac/wav/caf, not webm). Returns text, segments [{text,start,end}], locale.",
                json!({ "type": "object", "properties": { "path": { "type": "string" }, "locale": { "type": "string", "description": "e.g. en-US; default: the Mac's language" } }, "required": ["path"] }),
            ),
            (
                "ai.generate",
                "Apple Intelligence on-device language model (FoundationModels). Plain text, or JSON guided by a JSON Schema (object/string/number/integer/boolean/array/enum). Returns {text} or {json}.",
                json!({ "type": "object", "properties": { "prompt": { "type": "string" }, "instructions": { "type": "string" }, "schema": { "type": "object" }, "temperature": { "type": "number" } }, "required": ["prompt"] }),
            ),
            (
                "vision.ocr",
                "macOS on-device text recognition (Vision) on an image file in the workspace. Returns text and lines [{text, box (normalized, top-left origin), confidence}].",
                json!({ "type": "object", "properties": { "path": { "type": "string" }, "languages": { "type": "array", "items": { "type": "string" } } }, "required": ["path"] }),
            ),
            (
                "tts.speak",
                "Speak text aloud on the Mac (AVSpeechSynthesizer). Returns when finished.",
                json!({ "type": "object", "properties": { "text": { "type": "string" }, "voice": { "type": "string", "description": "voice id or language, e.g. en-US" }, "rate": { "type": "number", "description": "0..1" } }, "required": ["text"] }),
            ),
            (
                "tts.render",
                "Render speech to an audio file in the workspace (.m4a, .caf, .wav, .aiff).",
                json!({ "type": "object", "properties": { "text": { "type": "string" }, "path": { "type": "string" }, "voice": { "type": "string" } }, "required": ["text", "path"] }),
            ),
            (
                "translate",
                "Translate text on the Mac (Translation framework; only language pairs the user installed). from is detected when missing.",
                json!({ "type": "object", "properties": { "text": { "type": "string" }, "from": { "type": "string" }, "to": { "type": "string" } }, "required": ["text", "to"] }),
            ),
            (
                "nl.embed",
                "Sentence embeddings on the Mac (NaturalLanguage). Returns vectors[][] (512 dims) for texts[].",
                json!({ "type": "object", "properties": { "texts": { "type": "array", "items": { "type": "string" } }, "language": { "type": "string" } }, "required": ["texts"] }),
            ),
            (
                "nl.language",
                "Detect the language of a text on the Mac (NaturalLanguage). Returns language, confidence.",
                json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }),
            ),
        ]
        .into_iter()
        .map(|(m, d, sc)| Tool::new(crate::host_services::tool_name(m), d, schema(sc))),
    );
    v.extend(vec![
        Tool::new(
            "host_status",
            "Availability of the macOS host services (Apple Intelligence model, speech locales, translation, …) and the host modules with their methods.",
            schema(json!({ "type": "object", "properties": {} })),
        ),
        Tool::new(
            "host_call",
            "Call a method of a module's host part (§18.1: Swift compiled and run on the Mac after the user allowed it), or a standard-library method when module is omitted.",
            schema(json!({ "type": "object", "properties": { "module": { "type": "string" }, "method": { "type": "string" }, "params": { "type": "object" } }, "required": ["method"] })),
        ),
        Tool::new(
            "openapp_host_action",
            "Run a host action yourself. Chrome: view.open {view: \"<module>/<view>\", as: sheet|modal|inline|tab, title?, width?} (returns presented.id; a web view in it has placement \"present:<id>\"), view.close {view?}, web.navigate {placement?, url}, web.back/forward/reload {placement?}, guide.set, guide.clear, notify {text}, open_url {url} (system browser), composer.insert {text}. App setup for a module (pass \"module\": \"<id>\" in args): tray.set/clear, menu.set, window.open/close/focus/fullscreen, shortcut.register/unregister, dock.badge, notify.post, clipboard.write, files.reveal, service.*, pty.*, job.*, agent.run, secret.release, model.request {tier, reason}, … (openapp_describe → host_actions; native_shell shows the stored state: without an app window results say pending and apply when it opens). Panels and decisions (files.open/save/grant_folder, grants, providers, sessions) are the user's: put them on a control in a view.",
            schema(json!({ "type": "object", "properties": { "action": { "type": "string" }, "args": { "type": "object", "additionalProperties": true } }, "required": ["action"] })),
        ),
    ]);
    // §20 connect tools (secrets, net, OAuth, web control, guide)
    v.extend(crate::primitives::connect::mcp_tools());
    v.push(Tool::new(
        "openapp_undo",
        "Revert the last layout/theme change(s). Module code is not touched (use git for that).",
        schema(json!({
            "type": "object",
            "properties": { "steps": { "type": "integer", "minimum": 1, "default": 1 } }
        })),
    ));
    v
}

fn ok(v: Value) -> CallToolResult {
    CallToolResult::structured(v)
}

fn err(v: Value) -> CallToolResult {
    CallToolResult::structured_error(v)
}

fn err_msg(msg: impl Into<String>) -> CallToolResult {
    err(json!({ "ok": false, "error": msg.into() }))
}

impl KernelMcp {
    pub async fn dispatch(&self, name: &str, args: Map<String, Value>) -> CallToolResult {
        let k = &self.k;
        let s = |key: &str| args.get(key).and_then(Value::as_str).map(String::from);
        match name {
            "openapp_describe" => ok(k.describe()),
            "openapp_module_load" => match s("id") {
                Some(id) => match k.module_load(&id).await {
                    Ok(v) => ok(v),
                    Err(v) => err(v),
                },
                None => err_msg("missing \"id\""),
            },
            "openapp_module_unload" => match s("id") {
                Some(id) => {
                    let rm = args.get("remove_placements").and_then(Value::as_bool).unwrap_or(false);
                    match k.module_unload(&id, rm).await {
                        Ok(v) => ok(v),
                        Err(v) => err(v),
                    }
                }
                None => err_msg("missing \"id\""),
            },
            "openapp_layout" => match args.get("ops") {
                Some(Value::Array(ops)) => match k.layout_ops(ops) {
                    Ok(v) => ok(v),
                    Err(v) => err(v),
                },
                Some(Value::String(s)) => match serde_json::from_str::<Vec<Value>>(s) {
                    Ok(ops) => match k.layout_ops(&ops) {
                        Ok(v) => ok(v),
                        Err(v) => err(v),
                    },
                    Err(e) => err_msg(format!("\"ops\" must be an array of op objects ({e})")),
                },
                _ => err_msg("\"ops\" must be an array of op objects"),
            },
            "openapp_theme" => match args.get("tokens") {
                Some(Value::Object(t)) => match k.theme_set(t) {
                    Ok(v) => ok(v),
                    Err(v) => err(v),
                },
                _ => err_msg("\"tokens\" must be an object, e.g. {\"accent\": \"#0a7\"}"),
            },
            "openapp_call" => {
                let (Some(module), Some(tool)) = (s("module"), s("tool")) else {
                    return err_msg("\"module\" and \"tool\" are required");
                };
                let targs = match args.get("args") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(o)) => o.clone(),
                    Some(_) => return err_msg("\"args\" must be an object"),
                };
                k.research.log("agent", Kind::ToolCall, json!({ "via": "openapp_call", "module": module, "tool": tool }));
                match k.call_module_tool(&module, &tool, targs).await {
                    Ok(r) => {
                        let applied = k.after_module_call(&module, &r, false);
                        let is_error = r.is_error == Some(true);
                        let mut v = json!({ "ok": !is_error, "module": module, "tool": tool, "result": r });
                        if !applied.is_empty() {
                            v["directives_applied"] = json!(applied);
                        }
                        if is_error { err(v) } else { ok(v) }
                    }
                    Err(e) => err_msg(e),
                }
            }
            "openapp_snapshot" => {
                let (text, errors, problems) = k.snapshot_text();
                let mut r = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
                r.structured_content = Some(json!({ "text": text, "ui_errors": errors, "view_problems": problems }));
                r
            }
            "openapp_undo" => {
                let steps = args.get("steps").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
                let v = k.undo(steps, "agent");
                if v["ok"] == json!(true) { ok(v) } else { err(v) }
            }
            "openapp_host_action" => {
                let Some(action) = s("action") else { return err_msg("\"action\" is required") };
                // args, or the call's other keys when the agent put them at the top level
                let a = args.get("args").cloned().unwrap_or_else(|| {
                    let mut o = args.clone();
                    o.remove("action");
                    Value::Object(o)
                });
                match crate::primitives::connect::agent_action(k, &action, &a).await {
                    Ok(v) => ok(v),
                    Err(e) => err_msg(e),
                }
            }
            "openapp_guide" if args.get("clear").and_then(Value::as_bool) == Some(true) => {
                k.guide_clear();
                ok(json!({ "ok": true }))
            }
            t if crate::primitives::connect::method_of_tool(t).is_some() => {
                let method = crate::primitives::connect::method_of_tool(t).unwrap_or_default();
                match crate::primitives::connect::call(k, method, &Value::Object(args.clone()), "agent").await {
                    Ok(v) => ok(v),
                    Err(e) => err_msg(e),
                }
            }
            "host_status" => ok(k.host_services_info()),
            "host_call" => {
                let Some(method) = s("method") else { return err_msg("\"method\" is required") };
                let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
                match k.host_call(s("module").as_deref(), &method, params, "agent").await {
                    Ok(v) => ok(v),
                    Err(e) => err_msg(e),
                }
            }
            other if other.starts_with("host_") => {
                let Some(method) = crate::host_services::std_tool_method(other) else {
                    return err_msg(format!("unknown tool \"{other}\""));
                };
                match k.host_call(None, method, Value::Object(args.clone()), "agent").await {
                    Ok(v) => ok(v),
                    Err(e) => err_msg(e),
                }
            }
            other => match crate::primitives::call_tool(k, other, Value::Object(args.clone())).await {
                Some(Ok(v)) => ok(v),
                Some(Err(e)) => err_msg(e),
                None => err_msg(format!("unknown tool \"{other}\"")),
            },
        }
    }
}

impl ServerHandler for KernelMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("openapp", env!("CARGO_PKG_VERSION")).with_title("OpenApp kernel"))
            .with_instructions(INSTRUCTIONS)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        context.peer.set_peer_info(request.clone());
        let who = format!(
            "{} {} / protocol {}",
            request.client_info.name, request.client_info.version, request.protocol_version
        );
        log::info!("MCP client initialized: {who}");
        self.negotiate_initialize(&request)
    }

    async fn discover(&self, context: RequestContext<RoleServer>) -> Result<DiscoverResult, McpError> {
        let who = context
            .peer
            .peer_info()
            .map(|i| format!("{} {} / protocol {}", i.client_info.name, i.client_info.version, i.protocol_version))
            .unwrap_or_else(|| "stateless client / server/discover".into());
        log::info!("MCP client discovered the server: {who}");
        Ok(DiscoverResult::from_server_info(self.supported_protocol_versions().into_owned(), self.get_info()))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut all = tools();
        all.extend(crate::primitives::mcp_tools());
        Ok(ListToolsResult::with_all_items(all))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        Ok(self.dispatch(request.name.as_ref(), args).await.into())
    }
}

async fn auth(State(expected): State<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.as_bytes() == expected.as_bytes());
    let (method, path) = {
        let hdr = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        let p = format!("{} [version {}, mcp-method {}]", req.uri().path(), hdr("mcp-protocol-version"), hdr("mcp-method"));
        (req.method().clone(), p)
    };
    if ok {
        let resp = next.run(req).await;
        log::debug!("MCP {method} {path} -> {}", resp.status());
        resp
    } else {
        log::warn!("MCP {method} {path} -> 401 (missing or wrong bearer token)");
        (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response()
    }
}

/// `POST /host/<method>` (standard library) or `POST /host/<module>/<method>` (host module), JSON params,
/// `Authorization: Bearer $OPENAPP_HOST_TOKEN`. 200 → result JSON; errors → `{ "error" }`.
async fn host_route(
    State(k): State<Arc<Kernel>>,
    axum::extract::Path(rest): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let token = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let Some(caller) = k.host_caller(token) else {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "missing or wrong bearer token (use $OPENAPP_HOST_TOKEN)" }))).into_response();
    };
    let params: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": format!("body must be JSON params: {e}") }))).into_response(),
        }
    };
    let (module, method) = match rest.split_once('/') {
        Some((m, me)) => (Some(m.to_string()), me.to_string()),
        None => (None, rest.clone()),
    };
    if method == "status" && module.is_none() {
        return axum::Json(k.host_services_info()).into_response();
    }
    match k.host_call(module.as_deref(), &method, params, &caller).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => {
            let code = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, axum::Json(json!({ "error": e.message }))).into_response()
        }
    }
}

/// Start the server on 127.0.0.1:<random port>. Returns url + token.
pub async fn serve(k: Arc<Kernel>) -> anyhow::Result<McpEndpoint> {
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let ct = CancellationToken::new();
    let handler = KernelMcp { k: k.clone() };
    let service: StreamableHttpService<KernelMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.clone()),
    );
    let mcp = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(Arc::new(format!("Bearer {token}")), auth));
    // §18: host services for module servers (VM: through the 7801 forwarder), own bearer tokens
    let host = axum::Router::new().route("/host/{*rest}", axum::routing::post(host_route)).with_state(k.clone());
    // §21 primitive routes (e.g. the G1 channel WebSocket `/channel`)
    let router = mcp.merge(host).merge(crate::primitives::router(k));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).with_graceful_shutdown(async move { ct.cancelled_owned().await }).await {
            log::error!("MCP server stopped: {e}");
        }
    });
    Ok(McpEndpoint { url: format!("http://127.0.0.1:{port}/mcp"), token, port })
}
