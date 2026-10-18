//! §20 connect: secrets (Keychain), net.fetch / net.imap on the Mac, OAuth, web control of web views and the guide
//! strip. One table per surface: kernel methods ([`METHODS`], callable as `host_call`, `POST /host/<method>`, MCP
//! tools and host actions), the host actions ([`ACTIONS`], built-ins), the MCP tools ([`mcp_tools`], names derived
//! from the method) and the chrome actions the agent may run itself ([`agent_action`]).

use crate::research::Kind;
use super::{Builtin, Caller, src_params, src_str as get};
use crate::kernel::{HostError, Kernel};
use rmcp::model::Tool;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Kernel methods, in contract order.
pub const METHODS: &[&str] = &[
    "secret.request",
    "secret.delete",
    "secrets",
    "net.fetch",
    "net.imap",
    "oauth.authorize",
    "oauth.device",
    "web.read",
    "web.click",
    "web.fill",
    "web.highlight",
    "guide.set",
    "guide.clear",
];

pub fn is_method(m: &str) -> bool {
    METHODS.contains(&m)
}

/// Run a kernel method. `caller`: agent | module:<id> | user. Errors as text (MCP tools, view actions).
pub async fn call(k: &Arc<Kernel>, method: &str, params: &Value, caller: &str) -> Result<Value, String> {
    call_typed(k, method, params, caller).await.map_err(String::from)
}

/// [`call`] with typed errors (`POST /host/<method>` maps them to HTTP statuses).
pub async fn call_typed(k: &Arc<Kernel>, method: &str, params: &Value, caller: &str) -> Result<Value, HostError> {
    let p = if params.is_null() { &json!({}) } else { params };
    if !p.is_object() {
        return Err(HostError::bad_request("params must be an object"));
    }
    match method {
        "secret.request" => k.secret_request(p, caller).await,
        "secret.delete" => {
            let n = p.get("name").and_then(Value::as_str).ok_or(HostError::bad_request("secret.delete needs \"name\""))?;
            k.secret_delete(n, caller).await
        }
        "secrets" => Ok(json!({ "secrets": k.secrets.list() })),
        "net.fetch" => k.net_fetch(p, caller).await,
        "net.imap" => k.net_imap(p, caller).await,
        "oauth.authorize" => k.oauth_authorize(p, caller).await,
        "oauth.device" => k.oauth_device(p, caller).await,
        "web.read" | "web.click" | "web.fill" | "web.highlight" => k.web_control(&method[4..], p, caller).await,
        "guide.set" => Ok(k.guide_set(&[p]).map_err(HostError::bad_request)?),
        "guide.clear" => {
            k.guide_clear();
            Ok(json!({ "ok": true }))
        }
        other => Err(HostError::not_found(format!("unknown kernel method \"{other}\""))),
    }
}

/// A kernel method from a fire-and-forget host action: runs in the background, errors as toasts.
pub fn spawn_method(k: &Arc<Kernel>, method: &str, src: &[&Value]) -> Result<(), String> {
    let (k, m, params) = (k.clone(), method.to_string(), Value::Object(src_params(src)));
    tokio::spawn(async move {
        if let Err(e) = call(&k, &m, &params, "user").await {
            k.notify(&e);
        }
    });
    Ok(())
}

fn web_surface(k: &Arc<Kernel>, action: &str, src: &[&Value]) -> Result<(), String> {
    let placement = get(src, "placement").ok_or_else(|| format!("{action} needs \"placement\""))?;
    k.web_action(action, &placement, get(src, "url"))
}

pub const ACTIONS: &[Builtin] = &[
    Builtin { name: "web.navigate", args: "", keys: &["placement", "url"], run: |k, src| web_surface(k, "web.navigate", src) },
    Builtin { name: "web.back", args: "", keys: &["placement"], run: |k, src| web_surface(k, "web.back", src) },
    Builtin { name: "web.forward", args: "", keys: &["placement"], run: |k, src| web_surface(k, "web.forward", src) },
    Builtin { name: "web.reload", args: "", keys: &["placement"], run: |k, src| web_surface(k, "web.reload", src) },
    Builtin {
        name: "web.read",
        args: "{ placement? } → { url, title, text, elements: [{ ref, role, name, value? }] } (per-site grant)",
        keys: &[],
        run: |k, src| spawn_method(k, "web.read", src),
    },
    Builtin {
        name: "web.click",
        args: "{ placement?, ref } (create/enable/accept/grant/pay buttons are refused: highlight them for the user)",
        keys: &[],
        run: |k, src| spawn_method(k, "web.click", src),
    },
    Builtin { name: "web.fill", args: "{ placement?, ref, value }", keys: &[], run: |k, src| spawn_method(k, "web.fill", src) },
    Builtin { name: "web.highlight", args: "{ placement?, ref, note?, clear? }", keys: &[], run: |k, src| spawn_method(k, "web.highlight", src) },
    Builtin {
        name: "guide.set",
        args: "{ placement?, steps: [{ title, detail? }], current } — guide strip above the web view; Done sends \"Done: <title>\" to you",
        keys: &[],
        run: |k, src| k.guide_set(src).map(|_| ()),
    },
    Builtin {
        name: "guide.clear",
        args: "{}",
        keys: &[],
        run: |k, _| {
            k.guide_clear();
            Ok(())
        },
    },
    Builtin { name: "guide.done", args: "", keys: &[], run: |k, src| k.guide_done(src.iter().find_map(|v| v.get("step").and_then(Value::as_u64))) },
    Builtin {
        name: "secret.request",
        args: "{ name, label, kind: token|password|oauth-client, help?, url?, hosts?[] } — kernel secret card; value goes to the Keychain, you get { name, ok }",
        keys: &[],
        run: |k, src| spawn_method(k, "secret.request", src),
    },
    Builtin { name: "secret.delete", args: "{ name } — the user confirms", keys: &[], run: |k, src| spawn_method(k, "secret.delete", src) },
    Builtin {
        name: "net.fetch",
        args: "{ url, method?, headers?, body?|json?|body_base64?, auth?: { secret, scheme?: bearer|basic|header:<name>|query:<name>, username? } } → { status, headers, body|body_base64, json? } — HTTP on the Mac, credential injected",
        keys: &[],
        run: |k, src| spawn_method(k, "net.fetch", src),
    },
    Builtin {
        name: "net.imap",
        args: "{ host, secret, username, op: list|search|fetch, mailbox?, query?, uids?, part?: headers|text|full, limit? } — read-only IMAP (LOGIN or XOAUTH2)",
        keys: &[],
        run: |k, src| spawn_method(k, "net.imap", src),
    },
    Builtin {
        name: "oauth.authorize",
        args: "{ client: <oauth-client secret>, scopes[], provider?, token?, extra? } → { token, scopes } — PKCE + loopback in the system browser",
        keys: &[],
        run: |k, src| spawn_method(k, "oauth.authorize", src),
    },
    Builtin {
        name: "oauth.device",
        args: "{ client, scopes[], provider? } — device code flow (github, microsoft)",
        keys: &[],
        run: |k, src| spawn_method(k, "oauth.device", src),
    },
];

// ---------- MCP tools ----------

/// MCP tool name of a kernel method: `openapp_<method with . → _>` (`guide.set` is `openapp_guide`).
pub fn tool_name(method: &str) -> String {
    if method == "guide.set" { "openapp_guide".into() } else { super::agents::tool_name(&format!("openapp:{method}")) }
}

/// The kernel method behind an MCP tool name.
pub fn method_of_tool(tool: &str) -> Option<&'static str> {
    METHODS.iter().copied().find(|m| tool_name(m) == tool)
}

/// (method, description, input schema) of the methods that are MCP tools.
fn tool_defs() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "secret.request",
            "Ask the user for a secret (API token, app password, OAuth client JSON) with a trusted kernel card: masked input, optional link to where they get it. The value goes straight to the macOS Keychain; you get only {name, ok}. NEVER ask for secrets in chat. If the result says pending, call again with the same name to keep waiting. hosts = where the secret may be sent (e.g. [\"api.github.com\"]).",
            json!({ "type": "object", "properties": {
                "name": { "type": "string", "description": "secret name, e.g. github-token ([A-Za-z0-9._-])" },
                "label": { "type": "string", "description": "what the user sees, e.g. GitHub personal access token" },
                "kind": { "type": "string", "enum": ["token", "password", "oauth-client"], "default": "token" },
                "help": { "type": "string", "description": "one or two sentences: where to get it, which permissions" },
                "url": { "type": "string", "description": "page where the user creates it (opens in the system browser)" },
                "hosts": { "type": "array", "items": { "type": "string" }, "description": "hosts it may be sent to; \".example.com\" = any subdomain" },
                "username": { "type": "string" },
                "replace": { "type": "boolean", "description": "ask again even if it exists" }
            }, "required": ["name", "label"] }),
        ),
        (
            "secrets",
            "List the user's saved secrets (names, kinds, labels, providers, scopes, hosts, who used them) — never values.",
            json!({ "type": "object", "properties": {} }),
        ),
        (
            "secret.delete",
            "Delete a secret from the Keychain (the user confirms with a kernel card).",
            json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }),
        ),
        (
            "net.fetch",
            "HTTP request performed on the Mac (not in the VM) with a secret injected: auth {secret, scheme?: bearer (default for tokens/OAuth) | basic (default for passwords; username) | header:<Name> | query:<param>}. OAuth tokens refresh automatically. Network rules apply (new hosts ask the user). Returns {status, ok, headers, body | body_base64, json?}. Body cap 10 MB. Module servers: POST $OPENAPP_HOST_URL/net.fetch with the same JSON.",
            json!({ "type": "object", "properties": {
                "url": { "type": "string" }, "method": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "description": "string, or JSON value (sent as application/json)" },
                "json": { "description": "JSON body" }, "body_base64": { "type": "string" },
                "auth": { "type": "object", "properties": { "secret": { "type": "string" }, "scheme": { "type": "string" }, "username": { "type": "string" } }, "required": ["secret"] },
                "timeout_ms": { "type": "integer" }
            }, "required": ["url"] }),
        ),
        (
            "net.imap",
            "Read mail over IMAP from the Mac (TLS, port 993) with a password/app-password secret (LOGIN) or an OAuth token (XOAUTH2). op list → mailboxes; search {mailbox?, query? (IMAP SEARCH, e.g. UNSEEN, SINCE 1-Oct-2026, X-GM-RAW \"...\" on Gmail), limit?} → newest uids; fetch {mailbox?, uids, part: headers|text|full} → messages. Read-only.",
            json!({ "type": "object", "properties": {
                "host": { "type": "string" }, "port": { "type": "integer" }, "secret": { "type": "string" }, "username": { "type": "string" },
                "op": { "type": "string", "enum": ["list", "search", "fetch"] }, "mailbox": { "type": "string" }, "query": { "type": "string" },
                "uids": { "type": "array", "items": { "type": "integer" } }, "part": { "type": "string", "enum": ["headers", "text", "full"] }, "limit": { "type": "integer" }
            }, "required": ["host", "secret", "username", "op"] }),
        ),
        (
            "oauth.authorize",
            "Run OAuth (authorization code + PKCE, loopback redirect http://127.0.0.1:<port>/callback) in the user's system browser with an oauth-client secret; stores the token as an oauth-token secret and returns {token, scopes}. Presets: google, github, microsoft, slack, notion, dropbox, spotify (or auth_url + token_url). Waits up to 5 minutes for the user.",
            json!({ "type": "object", "properties": {
                "client": { "type": "string", "description": "oauth-client secret name (e.g. google-client from a captured download)" },
                "scopes": { "type": "array", "items": { "type": "string" } },
                "provider": { "type": "string" }, "token": { "type": "string", "description": "name for the token secret (default <client>-token)" },
                "extra": { "type": "object", "additionalProperties": { "type": "string" } },
                "auth_url": { "type": "string" }, "token_url": { "type": "string" }, "hosts": { "type": "array", "items": { "type": "string" } }
            }, "required": ["client", "scopes"] }),
        ),
        (
            "oauth.device",
            "OAuth device flow (GitHub, Microsoft): shows the user a code and opens the verification page; stores the token as an oauth-token secret.",
            json!({ "type": "object", "properties": { "client": { "type": "string" }, "scopes": { "type": "array", "items": { "type": "string" } }, "provider": { "type": "string" }, "token": { "type": "string" } }, "required": ["client", "scopes"] }),
        ),
        (
            "web.read",
            "Read the page in a web view: URL, title, visible text (≈8k chars) and interactive elements [{ref, role, name, value?, checked?, href?}]. The user grants control per site once. placement: \"present:<id>\", a placement id, \"<module>/<view>\", or omit for the top open web view.",
            json!({ "type": "object", "properties": { "placement": { "type": "string" }, "max": { "type": "integer" }, "max_text": { "type": "integer" } } }),
        ),
        (
            "web.click",
            "Click an element (ref from openapp_web_read) in a web view. Buttons that create, enable, accept terms, grant access, pay or delete are refused: highlight them and let the user click.",
            json!({ "type": "object", "properties": { "placement": { "type": "string" }, "ref": { "type": "string" } }, "required": ["ref"] }),
        ),
        (
            "web.fill",
            "Type a value into a field (input, textarea, select option, checkbox true/false) in a web view.",
            json!({ "type": "object", "properties": { "placement": { "type": "string" }, "ref": { "type": "string" }, "value": { "type": "string" } }, "required": ["ref", "value"] }),
        ),
        (
            "web.highlight",
            "Outline an element in a web view and scroll it into view, with an optional short note (\"Click Create\"). clear: true removes it.",
            json!({ "type": "object", "properties": { "placement": { "type": "string" }, "ref": { "type": "string" }, "note": { "type": "string" }, "clear": { "type": "boolean" } } }),
        ),
        (
            "guide.set",
            "Show the guide strip above a web view: steps [{title, detail?}] with the current step (0-based) and a Done button; Done sends you \"Done: <step title>\" as a user message. clear: true hides it.",
            json!({ "type": "object", "properties": {
                "placement": { "type": "string" }, "current": { "type": "integer" }, "clear": { "type": "boolean" },
                "steps": { "type": "array", "items": { "type": "object", "properties": { "title": { "type": "string" }, "detail": { "type": "string" } }, "required": ["title"] } }
            } }),
        ),
    ]
}

pub fn mcp_tools() -> Vec<Tool> {
    tool_defs()
        .into_iter()
        .map(|(m, d, schema)| {
            let schema = match schema {
                Value::Object(o) => Arc::new(o),
                _ => Arc::new(Map::new()),
            };
            Tool::new(tool_name(m), d, schema)
        })
        .collect()
}

// ---------- openapp_host_action ----------

/// Kernel chrome actions the agent may run itself through MCP `openapp_host_action`.
pub const AGENT_ACTIONS: &[&str] =
    &["view.open", "view.close", "web.navigate", "web.back", "web.forward", "web.reload", "guide.set", "guide.clear", "notify", "open_url", "composer.insert"];

/// Registry actions the agent may run through `openapp_host_action` (as caller `agent`, with `module` naming the
/// owner where the action needs one): configuring the user's app. Granted / Confirmed ones still show their card.
pub const AGENT_REGISTRY: &[&str] = &[
    // native shell (stored desired state; applied when the app window exists)
    "tray.set", "tray.clear", "menu.set", "window.open", "window.close", "window.focus", "window.fullscreen",
    "shortcut.register", "shortcut.unregister", "dock.badge", "notify.post", "clipboard.write", "clipboard.read",
    "files.reveal", "files.quicklook", "media.request", "media.camera",
    // VM services, terminals, displays, channels
    "service.status", "service.restart", "service.stop", "service.logs", "service.url",
    "pty.open", "pty.resize", "pty.write", "pty.close", "pty.list", "display.url", "channel.publish",
    // jobs, agent modules, background, secrets by reference
    "job.create", "job.update", "job.delete", "job.run_now", "job.list", "agent.run", "agent.cancel",
    "background.request", "app.login_item", "secret.release", "secret.revoke", "secret.generate", "models.refresh",
];

/// Agent-callable names that map to an MCP tool of the registry (same handler).
const AGENT_TOOL_ALIASES: &[(&str, &str)] = &[("model.request", "openapp_model_request")];

/// Actions that need the user (panels they answer, their decisions and choices) and why.
pub const USER_ONLY: &[(&str, &str)] = &[
    ("files.open", "a native Open panel the user answers"),
    ("files.save", "a native Save panel the user answers"),
    ("files.grant_folder", "a folder panel the user answers"),
    ("files.revoke_folder", "grant decisions are the user's"),
    ("files.drag_out", "starts from the user's mouse"),
    ("print", "a print panel the user answers"),
    ("share", "a share sheet the user answers"),
    ("model.set", "the user's model choice"),
    ("model.clear", "the user's model choice"),
    ("model.policy", "the user's escalation policy"),
    ("grant.revoke", "grant decisions are the user's"),
    ("harness.select", "the user's provider choice"),
    ("harness.login", "the user's sign-in"),
    ("harness.refresh", "the provider picker"),
    ("session.new", "the user's sessions"),
    ("session.open", "the user's sessions"),
    ("session.rename", "the user's sessions"),
    ("mode.set", "the user's chat/build mode"),
    ("turn.undo", "the user's undo"),
    ("prompt", "a message as the user"),
    ("audio.record", "the user's microphone button"),
    ("host.call", "use the host_call tool"),
    ("guide.done", "the user's Done button"),
];

/// What `openapp_host_action` runs, for its description and refusals.
pub fn agent_allowed() -> Vec<&'static str> {
    let mut v: Vec<&str> = AGENT_ACTIONS.to_vec();
    v.extend(AGENT_REGISTRY);
    v.extend(AGENT_TOOL_ALIASES.iter().map(|(a, _)| *a));
    v.push("safe.set (on: true)");
    v
}

fn refusal(action: &str) -> String {
    let allowed = agent_allowed().join(", ");
    match USER_ONLY.iter().find(|(a, _)| *a == action) {
        Some((_, why)) => format!(
            "\"{action}\" needs the user ({why}): put it on a control in a view (e.g. {{\"host\": \"{action}\", …}}) and let the user press it. openapp_host_action runs: {allowed}"
        ),
        None => format!("openapp_host_action does not run \"{action}\". It runs: {allowed}"),
    }
}

/// MCP `openapp_host_action`: a chrome action from [`AGENT_ACTIONS`] (with results), a registry action from
/// [`AGENT_REGISTRY`], or a tool alias; user-only actions are refused with what is allowed.
pub async fn agent_action(k: &Arc<Kernel>, action: &str, args: &Value) -> Result<Value, String> {
    if !AGENT_ACTIONS.contains(&action) {
        if let Some((_, tool)) = AGENT_TOOL_ALIASES.iter().find(|(a, _)| *a == action) {
            k.research.log("agent", Kind::HostAction, json!({ "action": action }));
            return super::call_tool(k, tool, args.clone()).await.unwrap_or_else(|| Err(format!("no tool {tool}")));
        }
        let safe_on = action == "safe.set" && args.get("on").and_then(Value::as_bool) == Some(true);
        if (AGENT_REGISTRY.contains(&action) || safe_on)
            && let Some(r) = super::call_action(k, action, args.clone(), Caller::new("agent")).await
        {
            return r;
        }
        if action == "safe.set" {
            return Err(format!("safe mode off needs the user (/safe off or the OpenApp menu). {}", refusal("")));
        }
        return Err(refusal(action));
    }
    k.research.log("agent", Kind::HostAction, json!({ "action": action }));
    match action {
        "view.open" => k.view_open(&[args]),
        "view.close" => k.view_close(args.get("view").and_then(Value::as_str)),
        "guide.set" => k.guide_set(&[args]),
        a if a.starts_with("web.") => {
            let placement = k.resolve_web_placement(args.get("placement").and_then(Value::as_str))?;
            k.web_action(a, &placement, args.get("url").and_then(Value::as_str).map(String::from))?;
            Ok(json!({ "ok": true, "placement": placement }))
        }
        a => k.host_action(a, &[args]).map(|_| json!({ "ok": true })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registry action and built-in is classified: agent-callable or user-only (a new primitive must choose).
    #[test]
    fn host_actions_classified() {
        let user: Vec<&str> = USER_ONLY.iter().map(|(a, _)| *a).collect();
        let mut names: Vec<&str> = super::super::all().iter().flat_map(|p| p.actions.iter().map(|a| a.name)).collect();
        names.extend(super::super::builtins().map(|b| b.name));
        for n in names {
            let agent = AGENT_ACTIONS.contains(&n) || AGENT_REGISTRY.contains(&n);
            let special = matches!(n, "safe.set") || is_method(n) || n.starts_with("web.");
            assert!(agent || user.contains(&n) || special, "host action {n} is neither agent-callable nor user-only");
            assert!(!(AGENT_REGISTRY.contains(&n) && user.contains(&n)), "{n} is in both lists");
        }
        for n in AGENT_REGISTRY {
            assert!(super::super::action(n).is_some(), "AGENT_REGISTRY lists unknown action {n}");
        }
        assert!(super::super::tool("openapp_model_request").is_some());
        assert!(refusal("files.open").contains("needs the user") && refusal("files.open").contains("tray.set"));
    }
}
