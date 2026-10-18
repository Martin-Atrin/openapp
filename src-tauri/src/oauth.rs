//! §20.2 OAuth for native apps: authorization code + PKCE (S256) with a loopback redirect
//! (`http://127.0.0.1:<random>/callback`) in the system browser, device flow where the provider has one,
//! refresh-token grant on expiry. Clients are `oauth-client` secrets; tokens are stored as `oauth-token`.
//! Provider presets carry endpoints only (no client ids).

use crate::research::Kind;
use crate::kernel::{HostError, Kernel};
use crate::net::{client, read_capped};
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub device_url: Option<&'static str>,
    /// extra authorization parameters
    pub extra: &'static [(&'static str, &'static str)],
    /// hosts the token may be sent to (net.fetch / net.imap)
    pub hosts: &'static [&'static str],
    /// client credentials as HTTP Basic at the token endpoint
    pub basic_auth: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "google",
        name: "Google",
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        device_url: Some("https://oauth2.googleapis.com/device/code"),
        extra: &[("access_type", "offline"), ("prompt", "consent")],
        hosts: &[".googleapis.com", "imap.gmail.com", "smtp.gmail.com"],
        basic_auth: false,
    },
    Preset {
        id: "github",
        name: "GitHub",
        auth_url: "https://github.com/login/oauth/authorize",
        token_url: "https://github.com/login/oauth/access_token",
        device_url: Some("https://github.com/login/device/code"),
        extra: &[],
        hosts: &["api.github.com", "uploads.github.com"],
        basic_auth: false,
    },
    Preset {
        id: "microsoft",
        name: "Microsoft",
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        device_url: Some("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode"),
        extra: &[],
        hosts: &["graph.microsoft.com", "outlook.office365.com", "outlook.office.com", "smtp.office365.com"],
        basic_auth: false,
    },
    Preset {
        id: "slack",
        name: "Slack",
        auth_url: "https://slack.com/oauth/v2/authorize",
        token_url: "https://slack.com/api/oauth.v2.access",
        device_url: None,
        extra: &[],
        hosts: &["slack.com", ".slack.com"],
        basic_auth: false,
    },
    Preset {
        id: "notion",
        name: "Notion",
        auth_url: "https://api.notion.com/v1/oauth/authorize",
        token_url: "https://api.notion.com/v1/oauth/token",
        device_url: None,
        extra: &[("owner", "user")],
        hosts: &["api.notion.com"],
        basic_auth: true,
    },
    Preset {
        id: "dropbox",
        name: "Dropbox",
        auth_url: "https://www.dropbox.com/oauth2/authorize",
        token_url: "https://api.dropboxapi.com/oauth2/token",
        device_url: None,
        extra: &[("token_access_type", "offline")],
        hosts: &["api.dropboxapi.com", "content.dropboxapi.com"],
        basic_auth: false,
    },
    Preset {
        id: "spotify",
        name: "Spotify",
        auth_url: "https://accounts.spotify.com/authorize",
        token_url: "https://accounts.spotify.com/api/token",
        device_url: None,
        extra: &[],
        hosts: &["api.spotify.com"],
        basic_auth: false,
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Random URL-safe string from `n` random bytes (UUID v4 = OS randomness).
pub fn random_string(n: usize) -> String {
    let mut bytes = Vec::with_capacity(n + 16);
    while bytes.len() < n {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    bytes.truncate(n);
    B64URL.encode(bytes)
}

/// PKCE verifier (43..128 chars) and S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = random_string(48); // 64 chars
    let challenge = B64URL.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Endpoints of a client: its own URLs, else the preset of `provider`, else explicit params.
struct Endpoints {
    provider: Option<String>,
    auth_url: Option<String>,
    token_url: String,
    device_url: Option<String>,
    extra: Vec<(String, String)>,
    hosts: Vec<String>,
    basic: bool,
}

fn endpoints(client: &Value, p: &Value) -> Result<Endpoints, String> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let provider = s(p, "provider").or_else(|| s(client, "provider"));
    let pre = provider.as_deref().and_then(preset);
    if let (Some(pv), None) = (&provider, pre)
        && s(p, "token_url").or_else(|| s(client, "token_url")).is_none()
    {
        return Err(format!("unknown provider \"{pv}\" (presets: {}); pass auth_url and token_url", PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ")));
    }
    let auth_url = s(p, "auth_url").or_else(|| s(client, "auth_url")).or_else(|| pre.map(|p| p.auth_url.to_string()));
    let token_url = s(p, "token_url").or_else(|| s(client, "token_url")).or_else(|| pre.map(|p| p.token_url.to_string())).ok_or("no token_url (set provider or token_url)")?;
    let device_url = s(p, "device_url").or_else(|| pre.and_then(|p| p.device_url.map(String::from)));
    let mut extra: Vec<(String, String)> = pre.map(|p| p.extra.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()).unwrap_or_default();
    if let Some(Value::Object(o)) = p.get("extra") {
        for (k, v) in o {
            let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
            extra.retain(|(a, _)| a != k);
            extra.push((k.clone(), v));
        }
    }
    let mut hosts: Vec<String> = pre.map(|p| p.hosts.iter().map(|h| h.to_string()).collect()).unwrap_or_default();
    for h in p.get("hosts").and_then(Value::as_array).cloned().unwrap_or_default() {
        if let Some(h) = h.as_str() {
            hosts.push(h.to_ascii_lowercase());
        }
    }
    for u in [&auth_url, &Some(token_url.clone())].into_iter().flatten() {
        let ok = u.starts_with("https://") || (crate::net::allow_loopback() && u.starts_with("http://127.0.0.1"));
        if !ok {
            return Err(format!("OAuth endpoints must be https: {u}"));
        }
    }
    Ok(Endpoints { provider, auth_url, token_url, device_url, extra, hosts, basic: pre.is_some_and(|p| p.basic_auth) })
}

/// Parse a token endpoint reply (JSON or form-encoded).
fn parse_reply(bytes: &[u8]) -> Value {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return v;
    }
    let mut m = Map::new();
    for (k, v) in url::form_urlencoded::parse(bytes) {
        m.insert(k.to_string(), json!(v.to_string()));
    }
    Value::Object(m)
}

const PAGE_OK: &str = "<!doctype html><meta charset=utf-8><title>OpenApp</title><body style=\"font:16px -apple-system,system-ui,sans-serif;display:grid;place-items:center;height:90vh;margin:0;color:#222\"><div style=\"text-align:center\"><h2 style=\"margin:0 0 8px\">You are signed in</h2><p style=\"color:#666\">You can close this tab and return to OpenApp.</p></div>";

fn page_err(msg: &str) -> String {
    let esc = msg.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<!doctype html><meta charset=utf-8><title>OpenApp</title><body style=\"font:16px -apple-system,system-ui,sans-serif;display:grid;place-items:center;height:90vh;margin:0;color:#222\"><div style=\"text-align:center\"><h2 style=\"margin:0 0 8px\">Sign-in did not finish</h2><p style=\"color:#666\">{esc}</p><p style=\"color:#666\">You can close this tab and return to OpenApp.</p></div>")
}

async fn respond(s: &mut tokio::net::TcpStream, status: &str, html: &str) {
    let r = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{html}", html.len());
    let _ = s.write_all(r.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Wait for the browser's redirect to `/callback`. Returns the `code` (state checked).
async fn wait_callback(listener: tokio::net::TcpListener, state: &str, deadline: Instant) -> Result<String, HostError> {
    loop {
        let left = deadline.checked_duration_since(Instant::now()).ok_or("sign-in timed out after 5 minutes")?;
        let (mut s, _) = tokio::time::timeout(left, listener.accept()).await.map_err(|_| "sign-in timed out after 5 minutes".to_string())?.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 16 * 1024];
        let mut n = 0;
        while n < buf.len() {
            let Ok(Ok(m)) = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf[n..])).await else { break };
            if m == 0 {
                break;
            }
            n += m;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let Ok(u) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            respond(&mut s, "400 Bad Request", &page_err("Bad request.")).await;
            continue;
        };
        if u.path() != "/callback" {
            respond(&mut s, "404 Not Found", "").await;
            continue;
        }
        let q: std::collections::HashMap<String, String> = u.query_pairs().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        if q.get("state").map(String::as_str) != Some(state) {
            respond(&mut s, "400 Bad Request", &page_err("The sign-in reply did not match this request (state check).")).await;
            continue; // not ours: keep waiting for the real reply
        }
        if let Some(e) = q.get("error") {
            let d = q.get("error_description").cloned().unwrap_or_default();
            respond(&mut s, "200 OK", &page_err(&format!("{e} {d}"))).await;
            return Err(HostError::upstream(format!("the provider returned an error: {e} {d}").trim()));
        }
        match q.get("code") {
            Some(c) if !c.is_empty() => {
                respond(&mut s, "200 OK", PAGE_OK).await;
                return Ok(c.clone());
            }
            _ => {
                respond(&mut s, "400 Bad Request", &page_err("No authorization code in the reply.")).await;
                return Err(HostError::upstream("no authorization code in the redirect"));
            }
        }
    }
}

/// §20.2 one token refresh at a time (per kernel, `k.prims`).
#[derive(Default)]
struct RefreshLock(tokio::sync::Mutex<()>);

impl Kernel {
    /// Open a URL in the system browser (tests can swap the opener).
    pub fn open_browser(&self, url: &str) -> Result<(), String> {
        if let Some(f) = self.url_opener.get() {
            return f(url);
        }
        crate::kernel::open_url(url)
    }

    async fn load_client(&self, name: &str) -> Result<Value, HostError> {
        let st = self.secrets_handle();
        let n = name.to_string();
        let (meta, raw) = tokio::task::spawn_blocking(move || st.get(&n)).await.map_err(|e| e.to_string())??;
        if meta["kind"] != "oauth-client" {
            return Err(HostError::bad_request(format!("\"{name}\" is not an OAuth client (kind {})", meta["kind"])));
        }
        Ok(serde_json::from_slice(&raw).map_err(|_| format!("OAuth client \"{name}\" is damaged; save it again"))?)
    }

    /// POST to a token endpoint (form body, JSON preferred). Egress rules apply to custom endpoints.
    async fn token_post(self: &Arc<Self>, ep: &Endpoints, client: &Value, pairs: Vec<(&str, String)>, actor: &str) -> Result<Value, HostError> {
        let u = url::Url::parse(&ep.token_url).map_err(|e| format!("bad token_url: {e}"))?;
        let host = u.host_str().unwrap_or("").to_ascii_lowercase();
        let is_preset_host = ep.provider.as_deref().and_then(preset).is_some_and(|p| p.token_url.contains(&format!("//{host}/")));
        if !is_preset_host {
            crate::net::egress_check(self, &host, u.port_or_known_default().unwrap_or(443), actor).await?;
        }
        let id = client["client_id"].as_str().unwrap_or("").to_string();
        let secret = client["client_secret"].as_str().map(String::from);
        let mut all: Vec<(&str, String)> = pairs;
        let mut rb = client_req(&ep.token_url);
        if ep.basic && secret.is_some() {
            let b = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{}", secret.clone().unwrap_or_default()));
            rb = rb.header("Authorization", format!("Basic {b}"));
        } else {
            all.push(("client_id", id));
            if let Some(s) = &secret {
                all.push(("client_secret", s.clone()));
            }
        }
        let refs: Vec<(&str, &str)> = all.iter().map(|(a, b)| (*a, b.as_str())).collect();
        let r = rb.body(form(&refs)).send().await.map_err(|e| format!("token request failed: {e}"))?;
        let status = r.status().as_u16();
        let v = parse_reply(&read_capped(r).await?);
        if v.get("error").is_some() || !(200..300).contains(&status) {
            let e = v["error"].as_str().unwrap_or("error").to_string();
            let d = v["error_description"].as_str().unwrap_or("").to_string();
            return Err(HostError::upstream(format!("token endpoint: {e} {d} (HTTP {status})").replace("  ", " ")));
        }
        Ok(v)
    }

    #[allow(clippy::too_many_arguments)]
    fn store_token(&self, name: &str, reply: &Value, prev: Option<&Value>, client_name: &str, ep: &Endpoints, scopes: &[String], actor: &str) -> Result<Value, String> {
        let access = reply["access_token"].as_str().ok_or("the token endpoint returned no access_token")?;
        let refresh = reply["refresh_token"].as_str().map(String::from).or_else(|| prev.and_then(|p| p["refresh_token"].as_str().map(String::from)));
        let expires_at = reply["expires_in"].as_i64().or_else(|| reply["expires_in"].as_str().and_then(|s| s.parse().ok())).map(|s| crate::util::now_secs() + s);
        let granted: Vec<String> = match reply["scope"].as_str() {
            Some(s) => s.split([' ', ',']).filter(|x| !x.is_empty()).map(String::from).collect(),
            None => prev.and_then(|p| p["scopes"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())).unwrap_or_else(|| scopes.to_vec()),
        };
        let stored = json!({
            "access_token": access, "refresh_token": refresh, "token_type": reply["token_type"], "expires_at": expires_at,
            "scopes": granted, "client": client_name, "token_url": ep.token_url, "provider": ep.provider,
        });
        let mut meta = Map::new();
        meta.insert("provider".into(), json!(ep.provider));
        meta.insert("scopes".into(), json!(granted));
        meta.insert("expires_at".into(), json!(expires_at));
        meta.insert("client".into(), json!(client_name));
        meta.insert("has_refresh_token".into(), json!(refresh.is_some()));
        if prev.is_none() {
            meta.insert("label".into(), json!(format!("{} account", ep.provider.as_deref().and_then(preset).map(|p| p.name).unwrap_or("OAuth"))));
            meta.insert("requested_by".into(), json!(actor));
            if !ep.hosts.is_empty() {
                meta.insert("hosts".into(), json!(ep.hosts));
                meta.insert("hosts_locked".into(), json!(true));
            }
        }
        self.secrets.put(name, "oauth-token", stored.to_string().as_bytes(), meta)?;
        self.emit_snapshot();
        Ok(json!({ "ok": true, "token": name, "scopes": granted, "expires_at": expires_at, "refresh": refresh.is_some() }))
    }

    /// `oauth.authorize {client, scopes[], provider?, extra?, token?, auth_url?, token_url?, port?}` → `{token, scopes}`.
    pub async fn oauth_authorize(self: &Arc<Self>, p: &Value, actor: &str) -> Result<Value, HostError> {
        let t0 = Instant::now();
        let client_name = p.get("client").and_then(Value::as_str).ok_or(HostError::bad_request("oauth.authorize needs \"client\" (an oauth-client secret name)"))?.to_string();
        let scopes: Vec<String> = p.get("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
        let token_name = p.get("token").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("{client_name}-token"));
        if !crate::secrets::valid_name(&token_name) {
            return Err(HostError::bad_request(format!("bad token name \"{token_name}\"")));
        }
        let client = self.load_client(&client_name).await?;
        let ep = endpoints(&client, p)?;
        let auth_url = ep.auth_url.clone().ok_or("no auth_url (set provider or auth_url)")?;
        let port = p.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| format!("cannot open the sign-in callback port: {e}"))?;
        let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| e.to_string())?.port());
        let (verifier, challenge) = pkce();
        let state = random_string(24);
        let mut u = url::Url::parse(&auth_url).map_err(|e| format!("bad auth_url: {e}"))?;
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("response_type", "code");
            q.append_pair("client_id", client["client_id"].as_str().unwrap_or(""));
            q.append_pair("redirect_uri", &redirect);
            if !scopes.is_empty() {
                q.append_pair("scope", &scopes.join(" "));
            }
            q.append_pair("state", &state);
            q.append_pair("code_challenge", &challenge);
            q.append_pair("code_challenge_method", "S256");
            for (k, v) in &ep.extra {
                q.append_pair(k, v);
            }
        }
        let who = ep.provider.as_deref().and_then(preset).map(|p| p.name).unwrap_or("the provider");
        self.research.log(actor, Kind::Oauth, json!({ "phase": "start", "client": client_name, "provider": ep.provider, "scopes": scopes, "flow": "pkce_loopback" }));
        self.open_browser(u.as_str())?;
        let sid = self.active_session();
        let line = self.chat_append_to(&sid, json!({ "kind": "system", "text": format!("Sign in with {who} in your browser to connect it. OpenApp is waiting (5 minutes)."), "url": u.as_str(), "linkLabel": "Open the sign-in page again" }));
        let res: Result<Value, HostError> = async {
            let code = wait_callback(listener, &state, Instant::now() + AUTH_TIMEOUT).await?;
            let reply = self
                .token_post(&ep, &client, vec![("grant_type", "authorization_code".into()), ("code", code), ("redirect_uri", redirect.clone()), ("code_verifier", verifier)], actor)
                .await?;
            Ok(self.store_token(&token_name, &reply, None, &client_name, &ep, &scopes, actor)?)
        }
        .await;
        let mut patch = Map::new();
        patch.insert("text".into(), json!(match &res {
            Ok(_) => format!("Connected {who}. The token is in your Keychain."),
            Err(e) => format!("{who} sign-in did not finish: {e}"),
        }));
        patch.insert("url".into(), Value::Null);
        self.chat_update_in(&sid, &line, patch);
        self.research.log(actor, Kind::Oauth, json!({ "phase": "end", "client": client_name, "ok": res.is_ok(), "ms": t0.elapsed().as_millis() as u64, "error": res.as_ref().err() }));
        res
    }

    /// `oauth.device {client, scopes[], provider?, token?}`: device authorization grant (GitHub, Microsoft, Google TV clients).
    pub async fn oauth_device(self: &Arc<Self>, p: &Value, actor: &str) -> Result<Value, HostError> {
        let t0 = Instant::now();
        let client_name = p.get("client").and_then(Value::as_str).ok_or(HostError::bad_request("oauth.device needs \"client\""))?.to_string();
        let scopes: Vec<String> = p.get("scopes").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
        let token_name = p.get("token").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("{client_name}-token"));
        let client = self.load_client(&client_name).await?;
        let ep = endpoints(&client, p)?;
        let device_url = ep.device_url.clone().ok_or("this provider has no device flow; use oauth.authorize")?;
        let du = url::Url::parse(&device_url).map_err(|e| e.to_string())?;
        if ep.provider.as_deref().and_then(preset).is_none() {
            crate::net::egress_check(self, du.host_str().unwrap_or(""), du.port_or_known_default().unwrap_or(443), actor).await?;
        }
        let cid = client["client_id"].as_str().unwrap_or("").to_string();
        let scope = scopes.join(" ");
        let r = client_req(&device_url).body(form(&[("client_id", &cid), ("scope", &scope)])).send().await.map_err(|e| format!("device request failed: {e}"))?;
        let d = parse_reply(&read_capped(r).await?);
        if let Some(e) = d["error"].as_str() {
            return Err(HostError::upstream(format!("device flow: {e} {}", d["error_description"].as_str().unwrap_or(""))));
        }
        let device_code = d["device_code"].as_str().ok_or("no device_code in the reply")?.to_string();
        let user_code = d["user_code"].as_str().unwrap_or("").to_string();
        let verify = d["verification_uri"].as_str().or_else(|| d["verification_url"].as_str()).unwrap_or("").to_string();
        let mut interval = d["interval"].as_u64().unwrap_or(5).max(1);
        let expires = Instant::now() + Duration::from_secs(d["expires_in"].as_u64().unwrap_or(900).min(900));
        let who = ep.provider.as_deref().and_then(preset).map(|p| p.name).unwrap_or("the provider");
        let sid = self.active_session();
        let line = self.chat_append_to(&sid, json!({ "kind": "system", "text": format!("Enter this code at {verify} to connect {who}:"), "code": user_code, "url": verify, "linkLabel": "Open" }));
        if verify.starts_with("https://") {
            let _ = self.open_browser(&verify);
        }
        self.research.log(actor, Kind::Oauth, json!({ "phase": "start", "client": client_name, "provider": ep.provider, "scopes": scopes, "flow": "device" }));
        let res = loop {
            if Instant::now() > expires {
                break Err(HostError::upstream("the device code expired; start again"));
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            match self.token_post(&ep, &client, vec![("grant_type", "urn:ietf:params:oauth:grant-type:device_code".into()), ("device_code", device_code.clone())], actor).await {
                Ok(reply) => break self.store_token(&token_name, &reply, None, &client_name, &ep, &scopes, actor).map_err(HostError::from),
                Err(e) if e.message.contains("authorization_pending") => continue,
                Err(e) if e.message.contains("slow_down") => {
                    interval += 5;
                    continue;
                }
                Err(e) => break Err(e),
            }
        };
        let mut patch = Map::new();
        patch.insert("text".into(), json!(match &res {
            Ok(_) => format!("Connected {who}. The token is in your Keychain."),
            Err(e) => format!("{who} sign-in did not finish: {e}"),
        }));
        patch.insert("code".into(), Value::Null);
        patch.insert("url".into(), Value::Null);
        self.chat_update_in(&sid, &line, patch);
        self.research.log(actor, Kind::Oauth, json!({ "phase": "end", "client": client_name, "ok": res.is_ok(), "ms": t0.elapsed().as_millis() as u64, "error": res.as_ref().err() }));
        res
    }

    /// A valid access token for an `oauth-token` secret (refreshes when it expires within 60 s, or `force`).
    pub async fn oauth_access_token(self: &Arc<Self>, name: &str, force: bool) -> Result<String, HostError> {
        let refresh = self.prims.get::<RefreshLock>();
        let _g = refresh.0.lock().await;
        let st = self.secrets_handle();
        let n = name.to_string();
        let (_, raw) = tokio::task::spawn_blocking(move || st.get(&n)).await.map_err(|e| e.to_string())??;
        let tok: Value = serde_json::from_slice(&raw).map_err(|_| format!("token \"{name}\" is damaged; connect again"))?;
        let access = tok["access_token"].as_str().unwrap_or("").to_string();
        let fresh = tok["expires_at"].as_i64().is_none_or(|e| e > crate::util::now_secs() + 60);
        if fresh && !force {
            return Ok(access);
        }
        let Some(refresh) = tok["refresh_token"].as_str().map(String::from) else {
            if force || !fresh {
                return Err(HostError::upstream(format!("token \"{name}\" expired and has no refresh token; run oauth.authorize again")));
            }
            return Ok(access);
        };
        let client_name = tok["client"].as_str().unwrap_or("").to_string();
        let client = self.load_client(&client_name).await?;
        let mut p = json!({ "token_url": tok["token_url"] });
        if let Some(pv) = tok["provider"].as_str() {
            p["provider"] = json!(pv);
        }
        let ep = endpoints(&client, &p)?;
        let t0 = Instant::now();
        let reply = self.token_post(&ep, &client, vec![("grant_type", "refresh_token".into()), ("refresh_token", refresh)], "kernel").await;
        self.research.log("kernel", Kind::Oauth, json!({ "phase": "refresh", "token": name, "ok": reply.is_ok(), "ms": t0.elapsed().as_millis() as u64, "error": reply.as_ref().err() }));
        let reply = reply.map_err(|e| format!("could not refresh \"{name}\": {e} (run oauth.authorize again)"))?;
        self.store_token(name, &reply, Some(&tok), &client_name, &ep, &[], "kernel")?;
        Ok(reply["access_token"].as_str().unwrap_or("").to_string())
    }
}

fn client_req(url: &str) -> reqwest::RequestBuilder {
    client()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256() {
        let (v, c) = pkce();
        assert!(v.len() >= 43 && v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(c, B64URL.encode(Sha256::digest(v.as_bytes())));
        // RFC 7636 appendix B vector
        assert_eq!(B64URL.encode(Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_ne!(random_string(24), random_string(24));
    }

    #[test]
    fn presets_and_replies() {
        for id in ["google", "github", "microsoft", "slack", "notion", "dropbox", "spotify"] {
            assert!(preset(id).is_some(), "{id}");
        }
        let e = endpoints(&json!({ "client_id": "x", "provider": "google" }), &json!({})).unwrap();
        assert!(e.auth_url.unwrap().contains("accounts.google.com") && e.extra.iter().any(|(k, v)| k == "access_type" && v == "offline"));
        assert!(endpoints(&json!({ "client_id": "x" }), &json!({ "provider": "nope" })).is_err());
        assert!(endpoints(&json!({ "client_id": "x" }), &json!({ "token_url": "http://evil.example/token" })).is_err());
        assert_eq!(parse_reply(b"access_token=abc&scope=repo&token_type=bearer")["access_token"], "abc");
        assert_eq!(parse_reply(br#"{"access_token":"j"}"#)["access_token"], "j");
    }
}
