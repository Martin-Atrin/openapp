//! §20.1 host network methods: `net.fetch` (HTTP on the host with credential injection) and `net.imap`
//! (read-only IMAP: list mailboxes, search, fetch headers/text by UID; LOGIN or XOAUTH2).
//!
//! Both apply the §17 egress policy per actor (same permission cards as the VM proxy), refuse the laptop's own
//! addresses (DNS answers included), send a secret only to hosts the user allowed for it, and scrub the
//! credential from anything they return. Research-logged without content or values.

use crate::research::Kind;
use crate::kernel::{HostError, Kernel};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_BODY: usize = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Tests only: let net.fetch / net.imap / OAuth reach 127.0.0.1. Debug builds only (ignored in release).
pub fn allow_loopback() -> bool {
    cfg!(debug_assertions) && std::env::var("OPENAPP_TEST_ALLOW_LOOPBACK").is_ok_and(|v| v == "1")
}

fn is_loopback_host(h: &str) -> bool {
    h == "localhost" || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// §17 policy for a host-side connection made on behalf of `actor` ("agent", "module:<id>", "user", "kernel").
pub async fn egress_check(k: &Arc<Kernel>, host: &str, port: u16, actor: &str) -> Result<(), HostError> {
    if allow_loopback() && is_loopback_host(host) {
        return Ok(());
    }
    let proxy_actor = match actor {
        "user" => "kernel".to_string(),
        a => a.replacen("module:", "module.", 1),
    };
    let (ok, reason) = crate::egress::decide(k, host, port, &proxy_actor).await;
    if ok { Ok(()) } else { Err(HostError::forbidden(format!("{host} is not allowed ({})", reason.as_str()))) }
}

/// DNS on the host that refuses local answers (no reaching the laptop's own services by name).
struct SafeResolver;

impl reqwest::dns::Resolve for SafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| allow_loopback() || !crate::egress::local_ip(&a.ip())).collect();
            if ok.is_empty() {
                return Err(format!("{host} resolves to a local address").into());
            }
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(SafeResolver))
            .connect_timeout(Duration::from_secs(20))
            .user_agent(concat!("OpenApp/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client")
    })
}

/// Literal IP targets: refuse local ones (resolver covers names).
fn check_literal(host: &str) -> Result<(), String> {
    if let Ok(ip) = host.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>()
        && crate::egress::local_ip(&ip)
        && !allow_loopback()
    {
        return Err(format!("{host} is a local address"));
    }
    if (host == "localhost" || host.ends_with(".localhost")) && !allow_loopback() {
        return Err(format!("{host} is a local address"));
    }
    Ok(())
}

/// Read a response body up to `MAX_BODY`.
pub async fn read_capped(mut r: reqwest::Response) -> Result<Vec<u8>, String> {
    if r.content_length().is_some_and(|n| n as usize > MAX_BODY) {
        return Err(format!("response is larger than {} MB", MAX_BODY / 1024 / 1024));
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await.map_err(|e| format!("reading the response failed: {e}"))? {
        out.extend_from_slice(&c);
        if out.len() > MAX_BODY {
            return Err(format!("response is larger than {} MB", MAX_BODY / 1024 / 1024));
        }
    }
    Ok(out)
}

/// The credential for a secret, as injected (and the strings to scrub from replies).
struct Cred {
    name: String,
    kind: String,
    scheme: String,
    header: Option<(String, String)>,
    query: Option<(String, String)>,
    scrub: Vec<String>,
}

fn scrub_text(s: &str, scrub: &[String]) -> String {
    let mut out = s.to_string();
    for v in scrub.iter().filter(|v| v.len() >= 4) {
        if out.contains(v.as_str()) {
            out = out.replace(v.as_str(), "[secret]");
        }
    }
    out
}

fn scrub_value(v: &mut Value, scrub: &[String]) {
    match v {
        Value::String(s) => *s = scrub_text(s, scrub),
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_value(x, scrub)),
        Value::Object(o) => o.values_mut().for_each(|x| scrub_value(x, scrub)),
        _ => {}
    }
}

impl Kernel {
    async fn secret_value(&self, name: &str) -> Result<(Value, Vec<u8>), String> {
        let name = name.to_string();
        // the Keychain may block (macOS can ask the user to allow access after a rebuild)
        let st = self.secrets_handle();
        tokio::task::spawn_blocking(move || st.get(&name)).await.map_err(|e| e.to_string())?
    }

    async fn credential(self: &Arc<Self>, auth: &Value, host: &str, actor: &str, force_refresh: bool) -> Result<Cred, HostError> {
        let name = auth.get("secret").and_then(Value::as_str).ok_or(HostError::bad_request("auth needs \"secret\" (a secret name)"))?.to_string();
        let (meta, raw) = self.secret_value(&name).await?;
        self.secret_host_check(&name, &meta, host, actor).await?;
        let kind = meta["kind"].as_str().unwrap_or("token").to_string();
        let value = match kind.as_str() {
            "oauth-token" => self.oauth_access_token(&name, force_refresh).await?,
            "oauth-client" => return Err(HostError::bad_request(format!("\"{name}\" is an OAuth client; run oauth.authorize to get a token and use that"))),
            _ => String::from_utf8(raw).map_err(|_| format!("secret \"{name}\" is not text"))?,
        };
        let default = if kind == "password" { "basic" } else { "bearer" };
        let scheme = auth.get("scheme").and_then(Value::as_str).unwrap_or(default).to_string();
        let mut scrub = vec![value.clone()];
        let (header, query) = match scheme.as_str() {
            "bearer" => (Some(("Authorization".to_string(), format!("Bearer {value}"))), None),
            "basic" => {
                let user = auth.get("username").and_then(Value::as_str).or_else(|| meta["username"].as_str()).map(String::from);
                let pair = match user {
                    Some(u) => format!("{u}:{value}"),
                    None if value.contains(':') => value.clone(),
                    None => return Err(HostError::bad_request("basic auth needs \"username\" (or a secret of the form user:password)")),
                };
                let b = B64.encode(pair.as_bytes());
                scrub.push(b.clone());
                (Some(("Authorization".to_string(), format!("Basic {b}"))), None)
            }
            s if s.starts_with("header:") && s.len() > 7 => (Some((s[7..].to_string(), value.clone())), None),
            s if s.starts_with("query:") && s.len() > 6 => (None, Some((s[6..].to_string(), value.clone()))),
            s => return Err(HostError::bad_request(format!("unknown auth scheme \"{s}\" (bearer, basic, header:<name>, query:<name>)"))),
        };
        Ok(Cred { name, kind, scheme, header, query, scrub })
    }

    /// `net.fetch {url, method?, headers?, body? | json? | body_base64?, auth?: {secret, scheme?, username?}, timeout_ms?}`
    /// → `{status, headers, body | body_base64, json?, url}`.
    pub async fn net_fetch(self: &Arc<Self>, p: &Value, actor: &str) -> Result<Value, HostError> {
        let t0 = Instant::now();
        let url_s = p.get("url").and_then(Value::as_str).ok_or(HostError::bad_request("net.fetch needs \"url\""))?;
        let mut url = url::Url::parse(url_s).map_err(|e| format!("bad url: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HostError::bad_request("net.fetch only does http(s)"));
        }
        let method_s = p.get("method").and_then(Value::as_str).unwrap_or("GET").to_ascii_uppercase();
        let mut method = reqwest::Method::from_bytes(method_s.as_bytes()).map_err(|_| format!("bad method {method_s}"))?;
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(h) = p.get("headers") {
            let o = h.as_object().ok_or("\"headers\" must be an object of strings")?;
            for (n, v) in o {
                let v = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => continue,
                    other => other.to_string(),
                };
                let name = reqwest::header::HeaderName::from_bytes(n.as_bytes()).map_err(|_| format!("bad header name {n}"))?;
                if matches!(name.as_str(), "host" | "content-length" | "connection" | "transfer-encoding") {
                    continue;
                }
                headers.insert(name, reqwest::header::HeaderValue::from_str(&v).map_err(|_| format!("bad value for header {n}"))?);
            }
        }
        let mut body: Option<Vec<u8>> = None;
        if let Some(j) = p.get("json").filter(|v| !v.is_null()) {
            body = Some(serde_json::to_vec(j).unwrap_or_default());
            headers.entry(reqwest::header::CONTENT_TYPE).or_insert(reqwest::header::HeaderValue::from_static("application/json"));
        } else if let Some(b) = p.get("body").filter(|v| !v.is_null()) {
            match b {
                Value::String(s) => body = Some(s.clone().into_bytes()),
                other => {
                    body = Some(serde_json::to_vec(other).unwrap_or_default());
                    headers.entry(reqwest::header::CONTENT_TYPE).or_insert(reqwest::header::HeaderValue::from_static("application/json"));
                }
            }
        } else if let Some(b) = p.get("body_base64").and_then(Value::as_str) {
            body = Some(B64.decode(b.trim()).map_err(|e| format!("bad body_base64: {e}"))?);
        }
        if body.as_ref().is_some_and(|b| b.len() > MAX_BODY) {
            return Err(HostError::bad_request("request body is larger than 10 MB"));
        }
        let timeout = Duration::from_millis(p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(60_000).clamp(1000, 300_000));
        let auth = p.get("auth").filter(|a| a.is_object()).cloned();
        let orig_host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let mut cred: Option<Cred> = None;
        let mut refreshed = false;
        let mut redirects = 0;
        let mut bytes_up = body.as_ref().map(|b| b.len()).unwrap_or(0) as u64;
        let resp = loop {
            let host = url.host_str().ok_or("url has no host")?.to_ascii_lowercase();
            let port = url.port_or_known_default().unwrap_or(443);
            check_literal(&host)?;
            egress_check(self, &host, port, actor).await?;
            let same_origin = host == orig_host;
            if let Some(a) = &auth
                && same_origin
                && cred.is_none()
            {
                cred = Some(self.credential(a, &host, actor, false).await?);
            }
            let mut req_url = url.clone();
            let mut hdrs = headers.clone();
            if let Some(c) = cred.as_ref().filter(|_| same_origin) {
                if let Some((n, v)) = &c.header {
                    let name = reqwest::header::HeaderName::from_bytes(n.as_bytes()).map_err(|_| format!("bad header name {n}"))?;
                    hdrs.insert(name, reqwest::header::HeaderValue::from_str(v).map_err(|_| "the secret is not a valid header value".to_string())?);
                }
                if let Some((n, v)) = &c.query {
                    req_url.query_pairs_mut().append_pair(n, v);
                }
            }
            let mut rb = client().request(method.clone(), req_url).headers(hdrs).timeout(timeout);
            if let Some(b) = &body {
                rb = rb.body(b.clone());
            }
            let r = rb.send().await.map_err(|e| {
                let msg = format!("request to {host} failed: {e}");
                cred.as_ref().map(|c| scrub_text(&msg, &c.scrub)).unwrap_or(msg)
            })?;
            let st = r.status().as_u16();
            if st == 401 && !refreshed && cred.as_ref().is_some_and(|c| c.kind == "oauth-token") {
                refreshed = true;
                if let Some(a) = &auth {
                    cred = Some(self.credential(a, &host, actor, true).await?);
                    continue;
                }
            }
            if matches!(st, 301 | 302 | 303 | 307 | 308) && redirects < MAX_REDIRECTS
                && let Some(loc) = r.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()) {
                    let next = url.join(loc).map_err(|e| format!("bad redirect: {e}"))?;
                    if !matches!(next.scheme(), "http" | "https") {
                        break r;
                    }
                    if st == 303 || ((st == 301 || st == 302) && method == reqwest::Method::POST) {
                        method = reqwest::Method::GET;
                        body = None;
                    }
                    url = next;
                    redirects += 1;
                    bytes_up += body.as_ref().map(|b| b.len()).unwrap_or(0) as u64;
                    continue;
                }
            break r;
        };
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let scrub: Vec<String> = cred.as_ref().map(|c| c.scrub.clone()).unwrap_or_default();
        let mut hmap = Map::new();
        for (n, v) in resp.headers() {
            let v = scrub_text(&String::from_utf8_lossy(v.as_bytes()), &scrub);
            match hmap.get_mut(n.as_str()) {
                Some(Value::String(prev)) => {
                    prev.push_str(if n == reqwest::header::SET_COOKIE { "\n" } else { ", " });
                    prev.push_str(&v);
                }
                _ => {
                    hmap.insert(n.as_str().to_string(), json!(v));
                }
            }
        }
        let bytes = read_capped(resp).await?;
        let mut out = json!({ "status": status, "ok": (200..300).contains(&status), "url": scrub_text(&final_url, &scrub), "headers": hmap });
        let textual = ctype.is_empty() || ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") || ctype.contains("javascript") || ctype.contains("x-www-form-urlencoded");
        match String::from_utf8(bytes.clone()) {
            Ok(s) if textual || !s.contains('\0') => {
                let s = scrub_text(&s, &scrub);
                if ctype.contains("json")
                    && let Ok(mut j) = serde_json::from_str::<Value>(&s)
                {
                    scrub_value(&mut j, &scrub);
                    out["json"] = j;
                }
                out["body"] = json!(s);
            }
            _ => out["body_base64"] = json!(B64.encode(&bytes)),
        }
        if let Some(c) = &cred {
            self.secrets_handle().note_use(&c.name, actor);
        }
        self.research.log(
            actor,
            Kind::NetFetch,
            json!({ "method": method.as_str(), "host": orig_host, "status": status, "redirects": redirects, "ms": t0.elapsed().as_millis() as u64,
                    "bytes_up": bytes_up, "bytes_down": bytes.len(), "secret": cred.as_ref().map(|c| c.name.clone()), "scheme": cred.as_ref().map(|c| c.scheme.clone()), "refreshed": refreshed }),
        );
        Ok(out)
    }

    /// `net.imap {host, port?, tls?, secret, username?, op: list|search|fetch, mailbox?, query?, uids?, part?, limit?}`
    pub async fn net_imap(self: &Arc<Self>, p: &Value, actor: &str) -> Result<Value, HostError> {
        let t0 = Instant::now();
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let host = s("host").ok_or(HostError::bad_request("net.imap needs \"host\" (e.g. imap.gmail.com)"))?.to_ascii_lowercase();
        let tls = p.get("tls").and_then(Value::as_bool).unwrap_or(true);
        if !tls && (!allow_loopback() || !is_loopback_host(&host)) {
            return Err(HostError::bad_request("net.imap needs TLS (port 993)"));
        }
        let port = p.get("port").and_then(Value::as_u64).map(|n| n as u16).unwrap_or(if tls { 993 } else { 143 });
        let op = s("op").unwrap_or_else(|| "list".into());
        if !["list", "search", "fetch"].contains(&op.as_str()) {
            return Err(HostError::bad_request("op must be list, search or fetch (net.imap is read-only)"));
        }
        let name = s("secret").ok_or(HostError::bad_request("net.imap needs \"secret\" (an app password or an OAuth token)"))?;
        check_literal(&host)?;
        egress_check(self, &host, port, actor).await?;
        let (meta, raw) = self.secret_value(&name).await?;
        self.secret_host_check(&name, &meta, &host, actor).await?;
        let kind = meta["kind"].as_str().unwrap_or("").to_string();
        let user = s("username").or_else(|| meta["username"].as_str().map(String::from)).ok_or(HostError::bad_request("net.imap needs \"username\" (the mail address)"))?;
        let (secret_text, xoauth) = match kind.as_str() {
            "oauth-token" => (self.oauth_access_token(&name, false).await?, true),
            "password" | "token" => (String::from_utf8(raw).map_err(|_| "secret is not text".to_string())?, false),
            k => return Err(HostError::bad_request(format!("net.imap cannot use a {k} secret"))),
        };
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("cannot resolve {host}: {e}"))?.collect();
        if addrs.is_empty() || (!allow_loopback() && addrs.iter().any(|a| crate::egress::local_ip(&a.ip()))) {
            return Err(HostError::forbidden(format!("{host} resolves to a local address")));
        }
        let tcp = tokio::time::timeout(Duration::from_secs(20), tokio::net::TcpStream::connect(&addrs[..]))
            .await
            .map_err(|_| format!("connect to {host}:{port} timed out"))?
            .map_err(|e| format!("connect to {host}:{port}: {e}"))?;
        let job = async {
            if tls {
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let sn = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| format!("bad host name: {e}"))?;
                let stream = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(sn, tcp).await.map_err(|e| format!("TLS with {host}: {e}"))?;
                imap_session(stream, &user, &secret_text, xoauth, &op, p).await
            } else {
                imap_session(tcp, &user, &secret_text, xoauth, &op, p).await
            }
        };
        let res = tokio::time::timeout(Duration::from_secs(120), job).await.map_err(|_| "IMAP timed out".to_string()).and_then(|r| r);
        let res = res.map_err(|e| scrub_text(&e, std::slice::from_ref(&secret_text)));
        if res.is_ok() {
            self.secrets_handle().note_use(&name, actor);
        }
        self.research.log(actor, Kind::NetImap, json!({ "host": host, "op": op, "ok": res.is_ok(), "ms": t0.elapsed().as_millis() as u64, "secret": name, "error": res.as_ref().err() }));
        Ok(res?)
    }
}

// ---------- minimal IMAP4rev1 client (read-only) ----------

struct Unit {
    /// response line with literals replaced by `\u{0}<index>`
    line: String,
    lits: Vec<Vec<u8>>,
}

struct Imap<S> {
    s: tokio::io::BufStream<S>,
    n: u32,
}

const IMAP_CAP: usize = 12 * 1024 * 1024;

impl<S: AsyncRead + AsyncWrite + Unpin> Imap<S> {
    async fn line(&mut self) -> Result<String, String> {
        let mut buf = Vec::new();
        let n = self.s.read_until(b'\n', &mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the IMAP server closed the connection".into());
        }
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    async fn unit(&mut self, total: &mut usize) -> Result<Unit, String> {
        let mut line = self.line().await?;
        let mut lits = Vec::new();
        loop {
            let lit = line.strip_suffix('}').and_then(|l| l.rsplit_once('{')).and_then(|(pre, n)| n.trim_end_matches('+').parse::<usize>().ok().map(|n| (pre.to_string(), n)));
            let Some((pre, len)) = lit else { break };
            *total += len;
            if *total > IMAP_CAP {
                return Err("IMAP response is too large (12 MB); fetch fewer messages".into());
            }
            let mut b = vec![0u8; len];
            self.s.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            line = format!("{pre}\u{0}{}{}", lits.len(), self.line().await?);
            lits.push(b);
        }
        *total += line.len();
        Ok(Unit { line, lits })
    }

    async fn greeting(&mut self) -> Result<(), String> {
        let mut t = 0;
        let u = self.unit(&mut t).await?;
        if u.line.starts_with("* OK") || u.line.starts_with("* PREAUTH") { Ok(()) } else { Err(format!("unexpected IMAP greeting: {}", u.line)) }
    }

    /// Send a command; collect untagged units until the tagged reply. `secret`: do not echo the command in errors.
    async fn cmd(&mut self, c: &str) -> Result<Vec<Unit>, String> {
        self.n += 1;
        let tag = format!("a{}", self.n);
        self.s.write_all(format!("{tag} {c}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        self.s.flush().await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let mut total = 0;
        loop {
            let u = self.unit(&mut total).await?;
            if let Some(rest) = u.line.strip_prefix(&format!("{tag} ")) {
                return if rest.starts_with("OK") { Ok(out) } else { Err(format!("IMAP: {rest}")) };
            }
            if u.line.starts_with('+') {
                // e.g. XOAUTH2 error details: answer with an empty line to get the tagged NO
                self.s.write_all(b"\r\n").await.map_err(|e| e.to_string())?;
                self.s.flush().await.map_err(|e| e.to_string())?;
                continue;
            }
            out.push(u);
        }
    }
}

fn quote(s: &str) -> Result<String, String> {
    if s.chars().any(|c| c == '\r' || c == '\n' || !c.is_ascii()) {
        return Err("IMAP strings must be single-line ASCII".into());
    }
    Ok(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// Take one IMAP string token (quoted, atom, NIL or literal placeholder) from the start of `s`.
fn take_string<'a>(s: &'a str, lits: &[Vec<u8>]) -> (Option<String>, &'a str) {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut it = rest.char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = it.next() {
                        out.push(n);
                    }
                }
                '"' => return (Some(out), &rest[i + 1..]),
                c => out.push(c),
            }
        }
        return (Some(out), "");
    }
    if let Some(rest) = s.strip_prefix('\u{0}') {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let idx: usize = rest[..end].parse().unwrap_or(0);
        return (lits.get(idx).map(|b| String::from_utf8_lossy(b).to_string()), &rest[end..]);
    }
    let end = s.find(' ').unwrap_or(s.len());
    let tok = &s[..end];
    (if tok.eq_ignore_ascii_case("NIL") { None } else { Some(tok.to_string()) }, &s[end..])
}

/// RFC 2047 encoded words (B and Q; UTF-8, US-ASCII, ISO-8859-1).
pub fn decode_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    let mut last_was_word = false;
    while let Some(i) = rest.find("=?") {
        let (before, after) = rest.split_at(i);
        let decoded = (|| {
            let body = &after[2..];
            let (charset, body) = body.split_once('?')?;
            let (enc, body) = body.split_once('?')?;
            let end = body.find("?=")?;
            let text = &body[..end];
            let bytes = match enc.to_ascii_uppercase().as_str() {
                "B" => B64.decode(text).ok()?,
                "Q" => {
                    let mut b = Vec::new();
                    let tb = text.as_bytes();
                    let mut j = 0;
                    while j < tb.len() {
                        match tb[j] {
                            b'_' => b.push(b' '),
                            b'=' if j + 2 < tb.len() => {
                                b.push(u8::from_str_radix(std::str::from_utf8(&tb[j + 1..j + 3]).ok()?, 16).ok()?);
                                j += 2;
                            }
                            c => b.push(c),
                        }
                        j += 1;
                    }
                    b
                }
                _ => return None,
            };
            let s = if charset.eq_ignore_ascii_case("iso-8859-1") || charset.eq_ignore_ascii_case("latin1") {
                bytes.iter().map(|&b| b as char).collect()
            } else {
                String::from_utf8_lossy(&bytes).to_string()
            };
            Some((s, 2 + charset.len() + 1 + enc.len() + 1 + end + 2))
        })();
        match decoded {
            Some((text, used)) => {
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = &after[used..];
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &after[2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn parse_headers(raw: &str) -> Map<String, Value> {
    let mut m = Map::new();
    let mut cur: Option<(String, String)> = None;
    let push = |m: &mut Map<String, Value>, (k, v): (String, String)| {
        let v = decode_words(v.trim());
        match m.get_mut(&k) {
            Some(Value::String(p)) => {
                p.push_str(", ");
                p.push_str(&v);
            }
            _ => {
                m.insert(k, json!(v));
            }
        }
    };
    for line in raw.split("\r\n").flat_map(|l| l.split('\n')) {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = cur.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some(c) = cur.take() {
            push(&mut m, c);
        }
        if let Some((k, v)) = line.split_once(':') {
            cur = Some((k.trim().to_ascii_lowercase(), v.to_string()));
        }
    }
    if let Some(c) = cur.take() {
        push(&mut m, c);
    }
    m
}

async fn imap_session<S: AsyncRead + AsyncWrite + Unpin>(stream: S, user: &str, secret: &str, xoauth: bool, op: &str, p: &Value) -> Result<Value, String> {
    let mut c = Imap { s: tokio::io::BufStream::new(stream), n: 0 };
    c.greeting().await?;
    if xoauth {
        let sasl = B64.encode(format!("user={user}\x01auth=Bearer {secret}\x01\x01"));
        c.cmd(&format!("AUTHENTICATE XOAUTH2 {sasl}")).await.map_err(|e| format!("sign-in failed: {e}"))?;
    } else {
        c.cmd(&format!("LOGIN {} {}", quote(user)?, quote(secret)?)).await.map_err(|e| format!("sign-in failed: {e}"))?;
    }
    let mailbox = p.get("mailbox").and_then(Value::as_str).unwrap_or("INBOX");
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 50) as usize;
    let out = match op {
        "list" => {
            let units = c.cmd("LIST \"\" \"*\"").await?;
            let mut boxes = Vec::new();
            for u in units.iter().filter(|u| u.line.starts_with("* LIST")) {
                let rest = &u.line[6..];
                let (flags, rest) = match (rest.find('('), rest.find(')')) {
                    (Some(a), Some(b)) if a < b => (rest[a + 1..b].split_whitespace().map(String::from).collect::<Vec<_>>(), &rest[b + 1..]),
                    _ => (Vec::new(), rest),
                };
                let (_delim, rest) = take_string(rest, &u.lits);
                let (name, _) = take_string(rest, &u.lits);
                if let Some(n) = name {
                    boxes.push(json!({ "name": n, "flags": flags }));
                }
            }
            json!({ "mailboxes": boxes })
        }
        "search" => {
            let q = p.get("query").and_then(Value::as_str).unwrap_or("ALL");
            if q.contains('\r') || q.contains('\n') {
                return Err("query must be one line".into());
            }
            let ex = c.cmd(&format!("EXAMINE {}", quote(mailbox)?)).await?;
            let exists = ex.iter().find_map(|u| u.line.strip_prefix("* ").and_then(|r| r.strip_suffix(" EXISTS")).and_then(|n| n.trim().parse::<u64>().ok()));
            let units = c.cmd(&format!("UID SEARCH {q}")).await?;
            let mut uids: Vec<u64> = units
                .iter()
                .filter_map(|u| u.line.strip_prefix("* SEARCH"))
                .flat_map(|r| r.split_whitespace().filter_map(|n| n.parse().ok()).collect::<Vec<u64>>())
                .collect();
            uids.sort_unstable();
            let total = uids.len();
            let newest: Vec<u64> = uids.into_iter().rev().take(limit).collect();
            json!({ "mailbox": mailbox, "exists": exists, "total": total, "uids": newest, "note": "newest first; at most `limit` (50) per call" })
        }
        _ => {
            let uids: Vec<u64> = p.get("uids").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
            if uids.is_empty() {
                return Err("fetch needs \"uids\" (from op: \"search\")".into());
            }
            let uids: Vec<u64> = uids.into_iter().take(limit).collect();
            let part = p.get("part").and_then(Value::as_str).unwrap_or("headers");
            let section = match part {
                "headers" => "BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID REPLY-TO)]",
                "text" => "BODY.PEEK[TEXT]",
                "full" => "BODY.PEEK[]",
                _ => return Err("part must be headers, text or full".into()),
            };
            c.cmd(&format!("EXAMINE {}", quote(mailbox)?)).await?;
            let set = uids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            let units = c.cmd(&format!("UID FETCH {set} (UID FLAGS INTERNALDATE RFC822.SIZE {section})")).await?;
            let mut msgs = Vec::new();
            for u in units.iter().filter(|u| u.line.starts_with("* ") && u.line.contains(" FETCH (")) {
                let l = &u.line;
                let num = |key: &str| l.find(key).and_then(|i| l[i + key.len()..].split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok());
                let flags: Vec<String> = l.find("FLAGS (").and_then(|i| l[i + 7..].split_once(')').map(|(f, _)| f.split_whitespace().map(String::from).collect())).unwrap_or_default();
                let date = l.find("INTERNALDATE \"").and_then(|i| l[i + 14..].split_once('"').map(|(d, _)| d.to_string()));
                let content = u.lits.first().map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
                let mut m = json!({ "uid": num("UID "), "flags": flags, "date": date, "size": num("RFC822.SIZE ") });
                if part == "headers" {
                    m["headers"] = Value::Object(parse_headers(&content));
                } else {
                    let cap = 1024 * 1024;
                    let truncated = content.len() > cap;
                    let mut t = content;
                    if truncated {
                        let mut cut = cap;
                        while !t.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        t.truncate(cut);
                    }
                    m[if part == "text" { "text" } else { "raw" }] = json!(t);
                    m["truncated"] = json!(truncated);
                }
                msgs.push(m);
            }
            json!({ "mailbox": mailbox, "messages": msgs })
        }
    };
    let _ = c.cmd("LOGOUT").await;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2047_and_headers() {
        assert_eq!(decode_words("=?UTF-8?B?SGVsbG8gV8O2cmxk?="), "Hello Wörld");
        assert_eq!(decode_words("Re: =?iso-8859-1?Q?caf=E9_ok?= now"), "Re: café ok now");
        assert_eq!(decode_words("=?UTF-8?Q?a?= =?UTF-8?Q?b?="), "ab");
        let h = parse_headers("Subject: Hello\r\n there\r\nFrom: A <a@x.org>\r\n\r\n");
        assert_eq!(h["subject"], "Hello there");
        assert_eq!(h["from"], "A <a@x.org>");
    }

    #[test]
    fn strings() {
        let lits = vec![b"Sent Items".to_vec()];
        assert_eq!(take_string(" \"a \\\"b\\\"\" rest", &lits).0.as_deref(), Some("a \"b\""));
        assert_eq!(take_string(" NIL x", &lits).0, None);
        assert_eq!(take_string(" \u{0}0", &lits).0.as_deref(), Some("Sent Items"));
        assert!(quote("x\r\ny").is_err());
        assert_eq!(scrub_text("token=abcd1234 ok", &["abcd1234".into()]), "token=[secret] ok");
    }
}
