//! §20.1 secrets: kernel-owned, values in the macOS Keychain, metadata in `<state>/secrets.json`.
//!
//! Keychain generic-password items: service `com.polygonface.openapp/<workspace-id>/<name>` (the prefix can be
//! changed with `OPENAPP_KEYCHAIN_PREFIX`, for tests), account `<name>`. Kinds: `token`, `password`,
//! `oauth-client` (JSON: client_id, client_secret?, auth_url?, token_url?, provider?), `oauth-token`
//! (JSON: access_token, refresh_token?, expires_at?, scopes, client, token_url, provider?), `keypair` (§21 G7
//! `secret.generate`: PKCS#8 PEM private key; the public part is in the metadata).
//!
//! Values never leave the kernel: not into chat, research, logs, the VM, MCP results or HTTP replies.
//! The agent and modules get names and metadata only, and use values through `net.fetch` / `net.imap`.

use crate::research::Kind;
use crate::util::MutexExt;
use crate::kernel::{HostError, Kernel};
use crate::state::now_ts;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const KINDS: &[&str] = &["token", "password", "oauth-client", "oauth-token", "keypair"];
/// Kinds the agent may ask the user to type (`oauth-token` only comes from `oauth.authorize`).
pub const REQUESTABLE: &[&str] = &["token", "password", "oauth-client"];
const MAX_VALUE: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

pub fn prefix() -> String {
    std::env::var("OPENAPP_KEYCHAIN_PREFIX").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "com.polygonface.openapp".into())
}

#[cfg(target_os = "macos")]
mod keychain {
    use security_framework::passwords;
    const NOT_FOUND: i32 = -25300;

    pub fn set(service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        passwords::set_generic_password(service, account, value).map_err(|e| format!("Keychain write failed ({})", e.code()))
    }
    pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        match passwords::get_generic_password(service, account) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(format!("Keychain read failed ({})", e.code())),
        }
    }
    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        match passwords::delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == NOT_FOUND => Ok(()),
            Err(e) => Err(format!("Keychain delete failed ({})", e.code())),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    pub fn set(_: &str, _: &str, _: &[u8]) -> Result<(), String> {
        Err("secrets need the macOS Keychain".into())
    }
    pub fn get(_: &str, _: &str) -> Result<Option<Vec<u8>>, String> {
        Err("secrets need the macOS Keychain".into())
    }
    pub fn delete(_: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Keychain + metadata for one workspace.
pub struct SecretStore {
    meta_path: PathBuf,
    ws_id: String,
    prefix: String,
    lock: Mutex<()>,
}

impl SecretStore {
    pub fn new(state_dir: &Path) -> Self {
        let ws_id = state_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "ws".into());
        Self { meta_path: state_dir.join("secrets.json"), ws_id, prefix: prefix(), lock: Mutex::new(()) }
    }

    pub fn service(&self, name: &str) -> String {
        format!("{}/{}/{name}", self.prefix, self.ws_id)
    }

    fn read(&self) -> Map<String, Value> {
        std::fs::read(&self.meta_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn write(&self, m: &Map<String, Value>) {
        let tmp = self.meta_path.with_extension("tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(m).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, &self.meta_path);
        }
    }

    /// Metadata of all secrets (never values), sorted by name.
    pub fn list(&self) -> Vec<Value> {
        self.read().into_iter().map(|(n, mut v)| {
            v["name"] = json!(n);
            v
        }).collect()
    }

    pub fn meta(&self, name: &str) -> Option<Value> {
        self.read().get(name).cloned().map(|mut v| {
            v["name"] = json!(name);
            v
        })
    }

    /// Store a value and its metadata (`created` is kept on replace). `extra` merges into the metadata.
    pub fn put(&self, name: &str, kind: &str, value: &[u8], extra: Map<String, Value>) -> Result<(), String> {
        if !valid_name(name) {
            return Err(format!("bad secret name \"{name}\" (letters, digits, . _ -; at most 64)"));
        }
        if !KINDS.contains(&kind) {
            return Err(format!("unknown secret kind \"{kind}\" ({})", KINDS.join(", ")));
        }
        if value.len() > MAX_VALUE {
            return Err("the secret is too large (64 KB at most)".into());
        }
        let _g = self.lock.locked();
        keychain::set(&self.service(name), name, value)?;
        let mut all = self.read();
        let now = now_ts();
        let mut m = match all.remove(name) {
            Some(Value::Object(o)) if o.get("kind").and_then(Value::as_str) == Some(kind) => o,
            _ => {
                let mut o = Map::new();
                o.insert("created".into(), json!(now));
                o
            }
        };
        m.insert("kind".into(), json!(kind));
        m.insert("updated".into(), json!(now));
        m.entry("used_by").or_insert_with(|| json!([]));
        for (k, v) in extra {
            if v.is_null() {
                m.remove(&k);
            } else {
                m.insert(k, v);
            }
        }
        all.insert(name.into(), Value::Object(m));
        self.write(&all);
        Ok(())
    }

    /// Value + metadata. Err when unknown or the Keychain item is gone.
    pub fn get(&self, name: &str) -> Result<(Value, Vec<u8>), String> {
        let meta = self.meta(name).ok_or_else(|| format!("no secret \"{name}\" (ask the user with secret.request)"))?;
        let v = keychain::get(&self.service(name), name)?.ok_or_else(|| format!("secret \"{name}\" is missing from the Keychain (ask the user again)"))?;
        Ok((meta, v))
    }

    /// Replace the value only (token refresh); metadata patch merged.
    pub fn update_value(&self, name: &str, value: &[u8], patch: Map<String, Value>) -> Result<(), String> {
        let kind = self.meta(name).and_then(|m| m["kind"].as_str().map(String::from)).ok_or_else(|| format!("no secret \"{name}\""))?;
        self.put(name, &kind, value, patch)
    }

    pub fn patch_meta(&self, name: &str, f: impl FnOnce(&mut Map<String, Value>)) {
        let _g = self.lock.locked();
        let mut all = self.read();
        if let Some(Value::Object(o)) = all.get_mut(name) {
            f(o);
            self.write(&all);
        }
    }

    /// Remember who used a secret (shown in host data `secrets`).
    pub fn note_use(&self, name: &str, actor: &str) {
        self.patch_meta(name, |m| {
            let mut used: Vec<Value> = m.get("used_by").and_then(Value::as_array).cloned().unwrap_or_default();
            if !used.iter().any(|u| u == actor) {
                used.push(json!(actor));
            }
            m.insert("used_by".into(), json!(used));
            m.insert("last_used".into(), json!(now_ts()));
        });
    }

    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let _g = self.lock.locked();
        keychain::delete(&self.service(name), name)?;
        let mut all = self.read();
        let had = all.remove(name).is_some();
        self.write(&all);
        Ok(had)
    }
}

/// Parse an OAuth client: a Google-style download (`{installed|web: {...}}`), a flat JSON object, or a bare client id.
/// Returns the normalized JSON stored in the Keychain and the non-secret metadata.
pub fn parse_oauth_client(raw: &str) -> Result<(Value, Map<String, Value>), String> {
    let t = raw.trim();
    let v: Value = if t.starts_with('{') {
        serde_json::from_str(t).map_err(|e| format!("the OAuth client is not valid JSON ({e})"))?
    } else if !t.is_empty() && !t.contains(char::is_whitespace) {
        json!({ "client_id": t })
    } else {
        return Err("paste the client JSON or the client ID".into());
    };
    let (inner, redirect) = match (v.get("installed"), v.get("web")) {
        (Some(i), _) => (i.clone(), "installed"),
        (None, Some(w)) => (w.clone(), "web"),
        _ => (v.clone(), "installed"),
    };
    let client_id = inner["client_id"].as_str().filter(|s| !s.is_empty()).ok_or("the OAuth client has no client_id")?.to_string();
    let s = |k: &str| inner.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let auth_url = s("auth_uri").or_else(|| s("auth_url"));
    let token_url = s("token_uri").or_else(|| s("token_url"));
    let provider = s("provider").or_else(|| {
        let a = auth_url.clone().unwrap_or_default();
        if a.contains("accounts.google.com") || client_id.ends_with(".apps.googleusercontent.com") { Some("google".into()) } else { None }
    });
    let stored = json!({
        "client_id": client_id,
        "client_secret": s("client_secret"),
        "auth_url": auth_url,
        "token_url": token_url,
        "provider": provider,
        "redirect": redirect,
    });
    let mut meta = Map::new();
    meta.insert("provider".into(), json!(provider));
    meta.insert("redirect".into(), json!(redirect));
    if let Some(p) = s("project_id") {
        meta.insert("project_id".into(), json!(p));
    }
    meta.insert("has_client_secret".into(), json!(s("client_secret").is_some()));
    Ok((stored, meta))
}

/// Does a downloaded file look like an OAuth client JSON?
pub fn looks_like_oauth_client(bytes: &[u8]) -> bool {
    if bytes.len() > 64 * 1024 {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else { return false };
    ["installed", "web"].iter().any(|k| v.get(*k).and_then(|i| i.get("client_id")).is_some_and(Value::is_string))
}

/// Open secret cards (per kernel, `k.prims`): card id → waiter.
#[derive(Default)]
pub struct SecretCards {
    pub waiters: Mutex<HashMap<String, SecretWaiter>>,
}

fn cards(k: &Kernel) -> Arc<SecretCards> {
    k.prims.get::<SecretCards>()
}

pub struct SecretWaiter {
    /// None while open; Some(saved) when answered (several tool calls may wait on one card)
    pub tx: tokio::sync::watch::Sender<Option<bool>>,
    pub session: String,
    pub name: String,
    pub kind: String,
    pub label: String,
    pub hosts: Vec<String>,
    pub actor: String,
    pub started: Instant,
}

fn host_list(v: Option<&Value>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for h in v.and_then(Value::as_array).cloned().unwrap_or_default() {
        let h = h.as_str().unwrap_or("").trim().to_ascii_lowercase();
        if h.is_empty() || h.len() > 253 || !h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Err(format!("bad host \"{h}\" in hosts (e.g. \"api.github.com\" or \".googleapis.com\")"));
        }
        out.push(h);
    }
    Ok(out)
}

/// `hosts` entry match: "api.x.com" exact, ".x.com" any subdomain (and x.com itself).
pub fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix('.') {
        Some(base) => host == base || host.ends_with(pattern),
        None => host == pattern,
    }
}

/// Words for a caller in cards.
pub fn actor_words(k: &Kernel, actor: &str) -> String {
    match actor {
        "agent" => k.lock().agent.name.clone(),
        "user" => "You".into(),
        "kernel" => "OpenApp".into(),
        a => match a.strip_prefix("module:").or_else(|| a.strip_prefix("module.")) {
            Some(id) => k.lock().modules.get(id).map(|m| m.name.clone()).unwrap_or_else(|| format!("Module {id}")),
            None => a.to_string(),
        },
    }
}

impl Kernel {
    /// Host data `secrets`: metadata only.
    pub fn secrets_list(&self) -> Vec<Value> {
        self.secrets.list()
    }

    /// `secret.request {name, label?, kind?, help?, url?, hosts?, replace?}` → kernel secret card → Keychain.
    /// Resolves when the user saves or cancels; returns `{name, ok}` (never the value).
    pub async fn secret_request(self: &Arc<Self>, p: &Value, actor: &str) -> Result<Value, HostError> {
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let name = s("name").ok_or(HostError::bad_request("secret.request needs \"name\" (e.g. \"github-token\")"))?;
        if !valid_name(&name) {
            return Err(HostError::bad_request(format!("bad secret name \"{name}\" (letters, digits, . _ -; at most 64)")));
        }
        let kind = s("kind").unwrap_or_else(|| "token".into());
        if !REQUESTABLE.contains(&kind.as_str()) {
            return Err(HostError::bad_request(format!("secret.request kind must be one of {} (oauth-token comes from oauth.authorize)", REQUESTABLE.join(", "))));
        }
        let url = s("url");
        if let Some(u) = &url
            && !(u.starts_with("https://") || u.starts_with("http://"))
        {
            return Err(HostError::bad_request("\"url\" must be an http(s) link"));
        }
        let hosts = host_list(p.get("hosts"))?;
        let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
        if !replace && let Some(m) = self.secrets.meta(&name) {
            return Ok(json!({ "ok": true, "name": name, "kind": m["kind"], "exists": true, "note": "already saved; pass replace: true to ask the user again" }));
        }
        let wait = Duration::from_secs(p.get("wait_s").and_then(Value::as_u64).unwrap_or(540).clamp(1, 1800));
        // a card for this name is already open: wait on it instead of asking twice
        let open = cards(self).waiters.locked().iter().find(|(_, w)| w.name == name).map(|(id, w)| (id.clone(), w.tx.subscribe()));
        let (id, mut rx) = match open {
            Some(o) => o,
            None => {
                let label = s("label").unwrap_or_else(|| name.clone());
                let sid = self.active_session();
                let who = actor_words(self, actor);
                let mut item = json!({ "kind": "secret", "title": label, "name": name, "secretKind": kind, "requestedBy": who });
                if let Some(h) = s("help") {
                    item["text"] = json!(h);
                }
                if let Some(u) = &url {
                    item["url"] = json!(u);
                }
                if !hosts.is_empty() {
                    item["hosts"] = json!(hosts);
                }
                let id = self.chat_append_to(&sid, item);
                self.research.log(actor, Kind::Secret, json!({ "phase": "shown", "id": id, "name": name, "kind": kind }));
                let (tx, rx) = tokio::sync::watch::channel(None);
                cards(self).waiters.locked().insert(
                    id.clone(),
                    SecretWaiter { tx, session: sid.clone(), name: name.clone(), kind: kind.clone(), label, hosts, actor: actor.to_string(), started: Instant::now() },
                );
                // the card closes by itself after 30 minutes
                let (k, cid) = (self.clone(), id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(REQUEST_TIMEOUT).await;
                    let w = cards(&k).waiters.locked().remove(&cid);
                    if let Some(w) = w {
                        let mut patch = Map::new();
                        patch.insert("resolved".into(), Value::Null);
                        k.chat_update_in(&w.session, &cid, patch);
                        let _ = w.tx.send(Some(false));
                    }
                });
                (id, rx)
            }
        };
        match tokio::time::timeout(wait, rx.wait_for(|v| v.is_some())).await {
            Ok(Ok(v)) if *v == Some(true) => Ok(json!({ "ok": true, "name": name, "kind": kind })),
            Ok(_) => Err(HostError::forbidden(format!("the user cancelled the request for \"{name}\" (do not ask again unless they want to)"))),
            Err(_) => Ok(json!({ "ok": false, "pending": true, "name": name, "request": id,
                "note": "the user has not answered yet; the card stays open. Call secret.request again with the same name to keep waiting." })),
        }
    }

    /// Shell command `submit_secret(request_id, value | null)`. The value goes straight to the Keychain.
    pub fn submit_secret(&self, request_id: &str, value: Option<String>) -> Result<(), String> {
        let w = cards(self).waiters.locked().remove(request_id).ok_or_else(|| format!("no open secret request \"{request_id}\""))?;
        let value = value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let Some(v) = value else {
            let mut patch = Map::new();
            patch.insert("resolved".into(), Value::Null);
            self.chat_update_in(&w.session, request_id, patch);
            self.research.log("user", Kind::Secret, json!({ "phase": "cancelled", "id": request_id, "name": w.name, "ms": w.started.elapsed().as_millis() as u64 }));
            let _ = w.tx.send(Some(false));
            return Ok(());
        };
        let mut extra = Map::new();
        extra.insert("label".into(), json!(w.label));
        extra.insert("requested_by".into(), json!(w.actor));
        if !w.hosts.is_empty() {
            // the card told the user "Used only with …": hold the secret to that
            extra.insert("hosts".into(), json!(w.hosts));
            extra.insert("hosts_locked".into(), json!(true));
        }
        let res = if w.kind == "oauth-client" {
            parse_oauth_client(&v).and_then(|(stored, meta)| {
                extra.extend(meta);
                self.secrets.put(&w.name, &w.kind, stored.to_string().as_bytes(), extra)
            })
        } else {
            self.secrets.put(&w.name, &w.kind, v.as_bytes(), extra)
        };
        if let Err(e) = res {
            // keep the card open so the user can fix it
            cards(self).waiters.locked().insert(request_id.to_string(), w);
            return Err(e);
        }
        let mut patch = Map::new();
        patch.insert("resolved".into(), json!("saved"));
        self.chat_update_in(&w.session, request_id, patch);
        self.research.log("user", Kind::Secret, json!({ "phase": "saved", "id": request_id, "name": w.name, "kind": w.kind, "ms": w.started.elapsed().as_millis() as u64 }));
        self.emit_snapshot();
        let _ = w.tx.send(Some(true));
        Ok(())
    }

    /// `secret.delete {name}`: always confirmed by the user with a kernel card.
    pub async fn secret_delete(&self, name: &str, actor: &str) -> Result<Value, HostError> {
        let meta = self.secrets.meta(name).ok_or_else(|| format!("no secret \"{name}\""))?;
        let label = meta["label"].as_str().unwrap_or(name).to_string();
        let who = actor_words(self, actor);
        let title = if actor == "user" || actor.starts_with("module:") { format!("Delete \"{label}\" from your Keychain?") } else { format!("{who} wants to delete \"{label}\" from your Keychain") };
        let req = crate::consent::ConsentReq::new("secret_delete", title, "Modules that use it stop working until you connect again.")
            .options(
                json!([
                    { "optionId": "delete", "name": "Delete", "kind": "delete" },
                    { "optionId": "keep", "name": "Keep", "kind": "reject_once" }
                ]),
                &["delete"],
            )
            .info(json!({ "name": name, "actor": actor }))
            .timeout(Duration::from_secs(300));
        if !crate::consent::ask(self, req).await.allowed {
            return Err(HostError::forbidden(format!("the user kept \"{name}\"")));
        }
        self.secrets.delete(name)?;
        self.research.log("user", Kind::Secret, json!({ "phase": "deleted", "name": name }));
        self.emit_snapshot();
        Ok(json!({ "ok": true, "deleted": name }))
    }

    /// May `actor` send secret `name` to `host`? Secrets with `hosts` only go there; others ask once per host
    /// (the answer is remembered in the secret's `hosts`).
    pub async fn secret_host_check(&self, name: &str, meta: &Value, host: &str, actor: &str) -> Result<(), HostError> {
        let hosts: Vec<String> = meta["hosts"].as_array().map(|a| a.iter().filter_map(|h| h.as_str().map(String::from)).collect()).unwrap_or_default();
        if hosts.iter().any(|p| host_matches(p, host)) {
            return Ok(());
        }
        if !hosts.is_empty() && meta["hosts_locked"] == true {
            return Err(HostError::forbidden(format!("secret \"{name}\" may only be sent to {} (not {host})", hosts.join(", "))));
        }
        let label = meta["label"].as_str().unwrap_or(name).to_string();
        let who = actor_words(self, actor);
        let text = format!(
            "OpenApp adds the secret to requests for {host} on your Mac; the app never sees the value.{}",
            if hosts.is_empty() { String::new() } else { format!(" So far it is used with {}.", hosts.join(", ")) }
        );
        let req = crate::consent::ConsentReq::new("secret_host", format!("{who} wants to send \"{label}\" to {host}"), text)
            .options(
                json!([
                    { "optionId": "allow", "name": format!("Allow for {host}"), "kind": "allow_always" },
                    { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
                ]),
                &["allow"],
            )
            .info(json!({ "name": name, "host": host, "actor": actor }))
            .timeout(Duration::from_secs(300));
        if !crate::consent::ask(self, req).await.allowed {
            return Err(HostError::forbidden(format!("the user did not allow sending \"{name}\" to {host}")));
        }
        let h = host.to_string();
        self.secrets.patch_meta(name, |m| {
            let mut list: Vec<Value> = m.get("hosts").and_then(Value::as_array).cloned().unwrap_or_default();
            list.push(json!(h));
            m.insert("hosts".into(), json!(list));
        });
        self.emit_snapshot();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_hosts() {
        assert!(valid_name("github-token") && valid_name("g.client_1"));
        assert!(!valid_name("") && !valid_name("-x") && !valid_name("a/b") && !valid_name(&"x".repeat(65)));
        assert!(host_matches(".googleapis.com", "gmail.googleapis.com") && host_matches(".googleapis.com", "googleapis.com"));
        assert!(!host_matches(".googleapis.com", "evilgoogleapis.com") && !host_matches("api.github.com", "github.com"));
    }

    #[test]
    fn oauth_client_json() {
        let raw = r#"{"installed":{"client_id":"123-abc.apps.googleusercontent.com","project_id":"p1","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","client_secret":"GOCSPX-x","redirect_uris":["http://localhost"]}}"#;
        assert!(looks_like_oauth_client(raw.as_bytes()));
        let (stored, meta) = parse_oauth_client(raw).unwrap();
        assert_eq!(stored["client_id"], "123-abc.apps.googleusercontent.com");
        assert_eq!(stored["provider"], "google");
        assert_eq!(meta["project_id"], "p1");
        assert!(!meta.values().any(|v| v == "GOCSPX-x"), "metadata never holds the client secret");
        assert!(!looks_like_oauth_client(b"{\"name\":\"x\"}"));
        assert_eq!(parse_oauth_client("Iv1.abc").unwrap().0["client_id"], "Iv1.abc");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_roundtrip() {
        // scratch prefix so tests never touch real items
        let dir = std::env::temp_dir().join(format!("oa-secrets-test-{}", std::process::id())).join("ws-test");
        std::fs::create_dir_all(&dir).unwrap();
        let st = SecretStore { meta_path: dir.join("secrets.json"), ws_id: format!("unit-{}", std::process::id()), prefix: "com.polygonface.openapp.test".into(), lock: Mutex::new(()) };
        st.put("t1", "token", b"value-1", Map::new()).unwrap();
        assert_eq!(st.get("t1").unwrap().1, b"value-1");
        assert!(!std::fs::read_to_string(dir.join("secrets.json")).unwrap().contains("value-1"));
        st.update_value("t1", b"value-2", Map::new()).unwrap();
        assert_eq!(st.get("t1").unwrap().1, b"value-2");
        assert!(st.delete("t1").unwrap());
        assert!(keychain::get(&st.service("t1"), "t1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
