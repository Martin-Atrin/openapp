//! §18 host services: bearer tokens for `POST /host/...`, module-server env, `host_call`.

use crate::research::Kind;
use super::*;

/// Error of a host endpoint (`host_call`, `POST /host/<method>`): the code decides the HTTP status. MCP tools and
/// view actions show only the message (`From<HostError> for String`). Plain string errors from lower layers are
/// [`HostCode::Upstream`] (a service or the network failed).
#[derive(Debug, Clone, PartialEq)]
pub struct HostError {
    pub code: HostCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCode {
    /// bad or missing params, a path outside the workspace (400)
    BadRequest,
    /// the user (or a grant) refused it (403)
    Forbidden,
    /// no such method, module host part or file (404)
    NotFound,
    /// the request clashes with the current state (409)
    Conflict,
    /// the service, sidecar or remote host failed (502)
    Upstream,
    /// a kernel failure (500)
    Internal,
}

impl HostError {
    pub fn new(code: HostCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(HostCode::BadRequest, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(HostCode::Forbidden, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(HostCode::NotFound, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(HostCode::Conflict, m)
    }
    pub fn upstream(m: impl Into<String>) -> Self {
        Self::new(HostCode::Upstream, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(HostCode::Internal, m)
    }

    pub fn status(&self) -> u16 {
        match self.code {
            HostCode::BadRequest => 400,
            HostCode::Forbidden => 403,
            HostCode::NotFound => 404,
            HostCode::Conflict => 409,
            HostCode::Upstream => 502,
            HostCode::Internal => 500,
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Research entries and JSON replies show the message.
impl serde::Serialize for HostError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.message)
    }
}

impl std::error::Error for HostError {}

impl From<String> for HostError {
    fn from(m: String) -> Self {
        Self::upstream(m)
    }
}

impl From<&str> for HostError {
    fn from(m: &str) -> Self {
        Self::upstream(m)
    }
}

impl From<HostError> for String {
    fn from(e: HostError) -> Self {
        e.message
    }
}

impl Kernel {
    /// Stable per-caller bearer token for `POST /host/...`.
    pub fn host_token(&self, caller: &str) -> String {
        let mut t = self.host_tokens.locked();
        if let Some((tok, _)) = t.iter().find(|(_, c)| c.as_str() == caller) {
            return tok.clone();
        }
        let tok = format!("oh_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        t.insert(tok.clone(), caller.to_string());
        tok
    }

    /// Caller of a host token (`agent` for the MCP token).
    pub fn host_caller(&self, token: &str) -> Option<String> {
        if self.mcp.get().is_some_and(|e| e.token == token) {
            return Some("agent".into());
        }
        self.host_tokens.locked().get(token).cloned()
    }

    /// `OPENAPP_HOST_URL` + `OPENAPP_HOST_TOKEN` for a module server (VM or host).
    pub fn host_env(&self, module: &str, in_vm: bool) -> Vec<(String, String)> {
        let port = self.mcp.get().map(|e| e.port).unwrap_or(0);
        let url = if in_vm { format!("http://127.0.0.1:{}/host", crate::vm::MCP_PORT) } else { format!("http://127.0.0.1:{port}/host") };
        let mut env = vec![("OPENAPP_HOST_URL".into(), url), ("OPENAPP_HOST_TOKEN".into(), self.host_token(&format!("module:{module}")))];
        if in_vm {
            // Agent modules run the VM's agent CLI headless with the same login (skill: "Missing means: build it").
            if crate::harness::selected(self).is_some_and(|s| s.adapter == crate::harness::Adapter::Pi) {
                env.push(("OPENAPP_AGENT_CLI".into(), crate::pi::agent_cli()));
                env.extend(crate::pi::guest_env(None).into_iter().filter(|(k, _)| k.starts_with("PI_")));
            } else {
                let dir = format!("{}/runtimes/codex/{}", crate::vm::GUEST_OPT, crate::codex::CODEX_VERSION);
                env.push(("OPENAPP_AGENT_CLI".into(), format!("{dir}/bin/codex exec --json --skip-git-repo-check")));
                env.push(("CODEX_HOME".into(), crate::codex::VM_CODEX_HOME.into()));
            }
        }
        env.extend(crate::primitives::env(self, module, in_vm));
        env
    }

    /// Call a host service: the standard library (`module` None) or a host module's method.
    /// Path params are workspace paths, mapped to host paths (refused outside the workspace).
    pub async fn host_call(self: &Arc<Self>, module: Option<&str>, method: &str, params: Value, caller: &str) -> Result<Value, HostError> {
        if module.is_none() && crate::primitives::connect::is_method(method) {
            return crate::primitives::connect::call_typed(self, method, &params, caller).await;
        }
        if module.is_none()
            && let Some(r) = crate::primitives::call_endpoint(self, method, params.clone(), caller).await
        {
            return r;
        }
        let t0 = std::time::Instant::now();
        let mut params = match params {
            Value::Object(o) => o,
            Value::Null => Map::new(),
            _ => return Err(HostError::bad_request("params must be an object")),
        };
        let (res, in_bytes) = match module {
            Some(m) => {
                let sc = self.lock().modules.get(m).and_then(|e| e.host.clone());
                let sc = sc.ok_or_else(|| HostError::not_found(format!("module \"{m}\" has no running host part (declare \"host\" in openapp.json, call openapp_module_load, and the user must allow it)")))?;
                let n = Value::Object(params.clone()).to_string().len() as u64;
                (sc.call(method, Value::Object(params), crate::host_services::timeout_for(method)).await, n)
            }
            None => {
                let sc = self.host_std.clone().ok_or("macOS host services are not available (openapp-macos was not built)")?;
                let known = crate::host_services::STD_METHODS.contains(&method);
                if !known {
                    return Err(HostError::not_found(format!(
                        "unknown host service \"{method}\" (standard library: {}; host modules: <module>/<method>)",
                        crate::host_services::STD_METHODS.join(", ")
                    )));
                }
                let mut original = None;
                let mut in_bytes = 0u64;
                for key in crate::host_services::PATH_PARAMS {
                    if let Some(p) = params.get(*key).and_then(Value::as_str).map(String::from) {
                        let output = method == "tts.render";
                        let host = crate::paths::map_path(&self.workspace, &p, !output).map_err(HostError::bad_request)?;
                        if !output {
                            in_bytes += std::fs::metadata(&host).map(|m| m.len()).unwrap_or(0);
                        }
                        params.insert(key.to_string(), json!(host.to_string_lossy()));
                        original = Some(p);
                    }
                }
                for v in params.values() {
                    match v {
                        Value::String(s) if original.as_deref() != Some(s.as_str()) => in_bytes += s.len() as u64,
                        Value::Array(a) => in_bytes += a.iter().filter_map(Value::as_str).map(|s| s.len() as u64).sum::<u64>(),
                        _ => {}
                    }
                }
                let mut r = sc.call(method, Value::Object(params), crate::host_services::timeout_for(method)).await;
                if let (Ok(v), Some(orig)) = (r.as_mut(), original)
                    && v.get("path").is_some()
                {
                    v["path"] = json!(orig);
                }
                (r, in_bytes)
            }
        };
        let out_bytes = res.as_ref().map(|v| v.to_string().len() as u64).unwrap_or(0);
        self.research.log(
            caller,
            Kind::HostService,
            json!({ "module": module, "method": method, "ok": res.is_ok(), "ms": t0.elapsed().as_millis() as u64, "in_bytes": in_bytes, "out_bytes": out_bytes, "error": res.as_ref().err() }),
        );
        Ok(res?)
    }
}
