//! Kernel egress proxy (contract §8): the app VM has no network device; every guest connection
//! goes through this HTTP proxy (CONNECT for https, absolute-form for plain http), reached from the
//! guest at 127.0.0.1:7802 (vsock-forwarded), or through the SOCKS5 listener ([`serve_socks`], RFC 1928
//! CONNECT, guest 127.0.0.1:7803) for non-HTTP protocols (IMAP, MTProto, MQTT, raw TCP). Both use the
//! same policy, cards, grants and research log. Per host:
//! - default allow: the provider's API/auth hosts, package registries, model hubs;
//! - a workspace grant in `<state>/grants.json` (outside the workspace) (`network.<host>`);
//! - otherwise a kernel permission card: Allow once · Allow for this workspace · Deny.
//!   "Allow once" admits the host for 2 minutes (one command's burst of connections), "Deny" refuses
//!   it for 1 minute (no card storm on retries); concurrent connections share one card.
//!
//! Loopback / link-local / unspecified targets are always refused (the laptop's own services are not
//! reachable from the VM through the proxy). Every connection is research-logged (`egress`).
//! The actor is the proxy user name the kernel puts into each guest process's proxy URL
//! (`agent`, `module.<id>`, `kernel`, else `guest`; SOCKS: the RFC 1929 user name); it is self-reported
//! by the guest. SOCKS targets are resolved here (socks5h); an IP-literal target gets its own card text.

use crate::research::Kind;
use crate::util::MutexExt;
use crate::consent::ConsentReq;
use crate::kernel::Kernel;
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

const ONCE_TTL: Duration = Duration::from_secs(120);
const DENY_TTL: Duration = Duration::from_secs(60);
const CARD_TIMEOUT: Duration = Duration::from_secs(600);
const HEAD_MAX: usize = 32 * 1024;

/// Default allowlist (§17). An entry starting with "." matches subdomains only.
pub const DEFAULT_ALLOW: &[&str] = &[
    // provider: Codex (OpenAI API, ChatGPT sign-in and backend)
    "api.openai.com",
    "auth.openai.com",
    "chatgpt.com",
    ".chatgpt.com",
    // provider: Pi with Claude (Anthropic API, Claude sign-in token endpoint)
    "api.anthropic.com",
    "platform.claude.com",
    // package registries
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    // model hubs (+ CDN)
    "huggingface.co",
    ".huggingface.co",
    ".hf.co",
    // §21 G3: Debian packages (apt through this proxy) and GitHub release downloads
    "deb.debian.org",
    "security.debian.org",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

pub fn default_allowed(host: &str) -> bool {
    DEFAULT_ALLOW.iter().any(|p| match p.strip_prefix('.') {
        Some(suffix) => host.len() > suffix.len() && host.ends_with(p),
        None => host == *p,
    })
}

#[derive(Default)]
pub struct EgressState {
    /// host -> (allowed, until)
    temp: Mutex<HashMap<String, (bool, Instant)>>,
    /// host -> waiters for the open card
    pending: Mutex<HashMap<String, Vec<oneshot::Sender<bool>>>>,
}

/// Egress decisions of this kernel (`k.prims`).
fn state(k: &Kernel) -> Arc<EgressState> {
    k.prims.get::<EgressState>()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    Default,
    Workspace,
    Once,
    User,
    Denied,
    DeniedRecently,
    Local,
    Invalid,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Default => "default",
            Reason::Workspace => "workspace_grant",
            Reason::Once => "allowed_once",
            Reason::User => "user",
            Reason::Denied => "denied",
            Reason::DeniedRecently => "denied_recently",
            Reason::Local => "local_address",
            Reason::Invalid => "invalid",
        }
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
}

/// Who asks, in words for the card.
fn actor_label(k: &Kernel, actor: &str) -> String {
    match actor {
        "agent" => k.lock().agent.name.clone(),
        "kernel" => "OpenApp".into(),
        a => match a.strip_prefix("module.") {
            Some(id) => k.lock().modules.get(id).map(|m| m.name.clone()).unwrap_or_else(|| format!("Module {id}")),
            None => "The app VM".into(),
        },
    }
}

/// Decide whether `actor` may reach `host:port` (may show a card and wait for the user).
pub async fn decide(k: &Arc<Kernel>, host: &str, port: u16, actor: &str) -> (bool, Reason) {
    if !valid_host(host) {
        return (false, Reason::Invalid);
    }
    let ip = host.parse::<IpAddr>().ok();
    if (ip.as_ref().is_some_and(local_ip) || host == "localhost" || host.ends_with(".localhost")) && !test_allow_local() {
        return (false, Reason::Local);
    }
    if ip.is_none() && default_allowed(host) {
        return (true, Reason::Default);
    }
    if k.grants.allowed("network", host) == Some(true) {
        return (true, Reason::Workspace);
    }
    {
        let st = state(k);
        let mut t = st.temp.locked();
        if let Some((allowed, until)) = t.get(host).cloned() {
            if Instant::now() < until {
                return (allowed, if allowed { Reason::Once } else { Reason::DeniedRecently });
            }
            t.remove(host);
        }
    }
    // one card per host: later connections wait for the same decision
    let (tx, rx) = oneshot::channel();
    let first = {
        let st = state(k);
        let mut p = st.pending.locked();
        let e = p.entry(host.to_string()).or_default();
        e.push(tx);
        e.len() == 1
    };
    if first {
        let k2 = k.clone();
        let host2 = host.to_string();
        let actor2 = actor.to_string();
        tokio::spawn(async move { ask(k2, host2, port, actor2).await });
    }
    match rx.await {
        Ok(true) => (true, Reason::User),
        _ => (false, Reason::Denied),
    }
}

/// Card title + text for a network target. An IP literal always shows its port and says that no name is known.
pub fn card_text(who: &str, host: &str, port: u16) -> (String, &'static str) {
    if host.parse::<IpAddr>().is_ok() {
        let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        return (
            format!("{who} wants to reach the IP address {target}"),
            "The app VM has no direct network. This connection goes to a raw IP address, not a named host, so OpenApp \
             cannot tell which service it is. Allow it only if you expect this app to talk to that address.",
        );
    }
    let target = if port == 443 || port == 80 { host.to_string() } else { format!("{host}:{port}") };
    (format!("{who} wants to reach {target}"), "The app VM has no direct network. OpenApp lets this host through only if you allow it.")
}

async fn ask(k: Arc<Kernel>, host: String, port: u16, actor: String) {
    let who = actor_label(&k, &actor);
    let (title, text) = card_text(&who, &host, port);
    let req = ConsentReq::new("network", title, text)
        .options(
            json!([
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "allow_workspace", "name": "Allow for this workspace", "kind": "allow_always" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
            &["allow_once", "allow_workspace"],
        )
        .info(json!({ "host": host, "port": port, "actor": actor }))
        .timeout(CARD_TIMEOUT);
    let d = crate::consent::ask(&k, req).await;
    match d.choice.as_deref() {
        Some("allow_workspace") => {
            k.grants.set("network", &host, true, json!({ "actor": actor }));
            k.emit_snapshot();
        }
        Some("allow_once") => {
            state(&k).temp.locked().insert(host.clone(), (true, Instant::now() + ONCE_TTL));
        }
        _ => {
            state(&k).temp.locked().insert(host.clone(), (false, Instant::now() + DENY_TTL));
        }
    }
    let waiters = state(&k).pending.locked().remove(&host).unwrap_or_default();
    for w in waiters {
        let _ = w.send(d.allowed);
    }
}

/// A parsed proxy request head.
#[derive(Debug, PartialEq)]
pub struct Head {
    pub method: String,
    pub host: String,
    pub port: u16,
    /// origin-form path for absolute-form requests ("" for CONNECT)
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub actor: String,
}

fn b64_decode(s: &str) -> Option<String> {
    crate::kernel::base64_decode(s).and_then(|b| String::from_utf8(b).ok())
}

fn split_host_port(authority: &str, default: u16) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default,
        };
        return Some((h.to_ascii_lowercase(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_ascii_lowercase(), p.parse().ok()?)),
        _ => Some((authority.to_ascii_lowercase(), default)),
    }
}

pub fn parse_head(raw: &str) -> Result<Head, String> {
    let mut lines = raw.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("bad request line {first:?}"));
    };
    let mut headers = Vec::new();
    for l in lines {
        if l.is_empty() {
            break;
        }
        if let Some((n, v)) = l.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let actor = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
        .and_then(|(_, v)| v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic ")))
        .and_then(|b| b64_decode(b.trim()))
        .map(|s| s.split(':').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty() && s.len() <= 80 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
        .unwrap_or_else(|| "guest".into());
    let (host, port, path) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(target, 443).ok_or("bad CONNECT authority")?;
        (h, p, String::new())
    } else {
        let rest = target.strip_prefix("http://").ok_or("only absolute http:// URLs (or CONNECT for https) go through this proxy")?;
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let auth = auth.rsplit('@').next().unwrap_or(auth);
        let (h, p) = split_host_port(auth, 80).ok_or("bad URL authority")?;
        (h, p, path)
    };
    let host = host.trim_end_matches('.').to_string();
    Ok(Head { method: method.to_string(), host, port, path, version: version.to_string(), headers, actor })
}

async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<(String, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("closed before a request".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok((String::from_utf8_lossy(&buf).to_string(), rest));
        }
        if buf.len() > HEAD_MAX {
            return Err("request head too large".into());
        }
    }
}

pub fn local_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_unspecified() || v.is_link_local() || v.is_broadcast() || v.is_multicast(),
        IpAddr::V6(v) => {
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (v.segments()[0] & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().is_some_and(|m| m.is_loopback() || m.is_unspecified() || m.is_link_local())
        }
    }
}

/// Debug builds only, `OPENAPP_TEST_EGRESS_LOCAL=1`: local targets go through the normal policy (cards) so the
/// VM suite can reach a test server on the Mac's loopback. Never in release builds.
fn test_allow_local() -> bool {
    cfg!(debug_assertions) && std::env::var("OPENAPP_TEST_EGRESS_LOCAL").is_ok_and(|v| v == "1")
}

/// Why an allowed connection could not be made.
enum UpErr {
    Resolve(String),
    DnsTimeout,
    Local,
    Connect(std::io::Error),
    ConnectTimeout,
}

impl UpErr {
    fn http(&self, host: &str, port: u16) -> (&'static str, String) {
        match self {
            UpErr::Resolve(e) => ("502 Bad Gateway", format!("cannot resolve {host}: {e}")),
            UpErr::DnsTimeout => ("504 Gateway Timeout", "DNS timeout".into()),
            UpErr::Local => ("403 Forbidden", format!("{host} resolves to a local address")),
            UpErr::Connect(e) => ("502 Bad Gateway", format!("connect {host}:{port}: {e}")),
            UpErr::ConnectTimeout => ("504 Gateway Timeout", "connect timeout".into()),
        }
    }
    /// RFC 1928 reply code.
    fn socks(&self) -> u8 {
        match self {
            UpErr::Resolve(_) | UpErr::DnsTimeout => 0x04,
            UpErr::Local => 0x02,
            UpErr::Connect(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => 0x05,
            UpErr::Connect(_) => 0x04,
            UpErr::ConnectTimeout => 0x06,
        }
    }
    fn log(&self) -> String {
        match self {
            UpErr::Resolve(e) => format!("resolve: {e}"),
            UpErr::DnsTimeout => "resolve timeout".into(),
            UpErr::Local => "local address".into(),
            UpErr::Connect(e) => format!("connect: {e}"),
            UpErr::ConnectTimeout => "connect timeout".into(),
        }
    }
}

/// Debug builds with [`test_allow_local`]: `OPENAPP_TEST_EGRESS_HOSTS=name=ip,…` answers those names (a test name
/// that the guest cannot resolve itself, mapped to the Mac's loopback).
fn test_host(host: &str) -> Option<IpAddr> {
    if !test_allow_local() {
        return None;
    }
    let map = std::env::var("OPENAPP_TEST_EGRESS_HOSTS").ok()?;
    map.split(',').filter_map(|e| e.split_once('=')).find(|(n, _)| n.trim() == host).and_then(|(_, ip)| ip.trim().parse().ok())
}

/// Resolve on the host (refusing the laptop's own addresses) and connect.
async fn connect_upstream(host: &str, port: u16) -> Result<tokio::net::TcpStream, UpErr> {
    let lookup = async {
        match test_host(host) {
            Some(ip) => Ok(vec![SocketAddr::new(ip, port)]),
            None => tokio::net::lookup_host((host, port)).await.map(|a| a.collect::<Vec<_>>()),
        }
    };
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(Duration::from_secs(10), lookup).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Err(UpErr::Resolve(e.to_string())),
        Err(_) => return Err(UpErr::DnsTimeout),
    };
    if addrs.is_empty() {
        return Err(UpErr::Resolve("no addresses".into()));
    }
    if addrs.iter().any(|a| local_ip(&a.ip())) && !test_allow_local() {
        return Err(UpErr::Local);
    }
    match tokio::time::timeout(Duration::from_secs(20), tokio::net::TcpStream::connect(&addrs[..])).await {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(UpErr::Connect(e)),
        Err(_) => Err(UpErr::ConnectTimeout),
    }
}

async fn reply<S: AsyncWrite + Unpin>(s: &mut S, status: &str, msg: &str) {
    let body = format!("OpenApp egress proxy: {msg}\n");
    let r = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = s.write_all(r.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Serve one proxy connection from the guest.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(k: Arc<Kernel>, mut s: S) {
    let t0 = Instant::now();
    let Ok(Ok((raw, rest))) = tokio::time::timeout(Duration::from_secs(30), read_head(&mut s)).await else {
        return; // probes and idle connections are not logged
    };
    let head = match parse_head(&raw) {
        Ok(h) => h,
        Err(e) => {
            reply(&mut s, "400 Bad Request", &e).await;
            k.research.log("kernel", Kind::Egress, json!({ "decision": "invalid", "error": e }));
            return;
        }
    };
    let log = |k: &Kernel, decision: &str, reason: Reason, up: u64, down: u64, err: Option<String>| {
        k.research.log(
            "kernel",
            Kind::Egress,
            json!({
                "host": head.host, "port": head.port, "method": head.method, "actor": head.actor,
                "decision": decision, "reason": reason.as_str(), "bytes_up": up, "bytes_down": down,
                "ms": t0.elapsed().as_millis() as u64, "error": err
            }),
        );
    };
    let (allowed, reason) = decide(&k, &head.host, head.port, &head.actor).await;
    if !allowed {
        reply(&mut s, "403 Forbidden", &format!("{} is not allowed ({})", head.host, reason.as_str())).await;
        log(&k, "deny", reason, 0, 0, None);
        return;
    }
    // resolve on the host; refuse the laptop's own addresses
    let mut up = match connect_upstream(&head.host, head.port).await {
        Ok(t) => t,
        Err(e) => {
            let (status, msg) = e.http(&head.host, head.port);
            reply(&mut s, status, &msg).await;
            if matches!(e, UpErr::Local) {
                log(&k, "deny", Reason::Local, 0, 0, None);
            } else {
                log(&k, "allow", reason, 0, 0, Some(e.log()));
            }
            return;
        }
    };
    let mut sent_extra = 0u64;
    if head.method.eq_ignore_ascii_case("CONNECT") {
        if s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
    } else {
        // absolute-form → origin-form, one request per upstream connection
        let mut out = format!("{} {} {}\r\n", head.method, head.path, head.version);
        for (n, v) in &head.headers {
            let l = n.to_ascii_lowercase();
            if l == "proxy-authorization" || l == "proxy-connection" || l == "connection" || l == "keep-alive" {
                continue;
            }
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        if up.write_all(out.as_bytes()).await.is_err() {
            return;
        }
        sent_extra += out.len() as u64;
    }
    if !rest.is_empty() {
        let _ = up.write_all(&rest).await;
        sent_extra += rest.len() as u64;
    }
    let (a, b, err) = relay(&mut s, &mut up).await;
    log(&k, "allow", reason, a + sent_extra, b, err);
}

/// Copy both directions of a tunnel until both end: (bytes up, bytes down, first error). Unlike
/// `copy_bidirectional`, the counts survive an error, so a reset tunnel is logged with what it moved.
async fn relay<A: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin>(a: &mut A, b: &mut B) -> (u64, u64, Option<String>) {
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let ((up, e1), (down, e2)) = tokio::join!(pump(&mut ar, &mut bw), pump(&mut br, &mut aw));
    (up, down, e1.or(e2))
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W) -> (u64, Option<String>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        match r.read(&mut buf).await {
            Ok(0) => {
                let _ = w.shutdown().await;
                return (n, None);
            }
            Ok(m) => {
                if let Err(e) = w.write_all(&buf[..m]).await {
                    return (n, Some(format!("write: {e}")));
                }
                n += m as u64;
            }
            Err(e) => {
                let _ = w.shutdown().await;
                return (n, Some(format!("read: {e}")));
            }
        }
    }
}

// ---------- SOCKS5 (RFC 1928 CONNECT, RFC 1929 user name = actor) ----------

/// A parsed SOCKS5 CONNECT target.
#[derive(Debug, PartialEq)]
pub struct SocksTarget {
    pub host: String,
    pub port: u16,
    /// the client sent an IPv4/IPv6 address (no name)
    pub ip_literal: bool,
}

fn valid_actor(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Method negotiation (+ RFC 1929 sub-negotiation): returns the actor. No-auth is accepted ("guest").
async fn socks_hello<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<String, String> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).await.map_err(|e| e.to_string())?;
    if h[0] != 5 {
        return Err(format!("not SOCKS5 (version {})", h[0]));
    }
    let mut methods = vec![0u8; h[1] as usize];
    s.read_exact(&mut methods).await.map_err(|e| e.to_string())?;
    if methods.contains(&0x02) {
        s.write_all(&[5, 0x02]).await.map_err(|e| e.to_string())?;
        let mut v = [0u8; 2];
        s.read_exact(&mut v).await.map_err(|e| e.to_string())?;
        let mut user = vec![0u8; v[1] as usize];
        s.read_exact(&mut user).await.map_err(|e| e.to_string())?;
        let mut pl = [0u8; 1];
        s.read_exact(&mut pl).await.map_err(|e| e.to_string())?;
        let mut pass = vec![0u8; pl[0] as usize];
        s.read_exact(&mut pass).await.map_err(|e| e.to_string())?;
        // the password is not checked: the user name only names the actor (as the HTTP proxy's user part)
        s.write_all(&[1, 0]).await.map_err(|e| e.to_string())?;
        let u = String::from_utf8_lossy(&user).to_string();
        return Ok(if valid_actor(&u) { u } else { "guest".into() });
    }
    if methods.contains(&0x00) {
        s.write_all(&[5, 0x00]).await.map_err(|e| e.to_string())?;
        return Ok("guest".into());
    }
    let _ = s.write_all(&[5, 0xff]).await;
    Err("no acceptable SOCKS auth method".into())
}

/// Read the request; `Err((reply code, message))` for unsupported commands / address types.
async fn socks_request<S: AsyncRead + Unpin>(s: &mut S) -> Result<SocksTarget, (u8, String)> {
    let io = |e: std::io::Error| (0x01u8, e.to_string());
    let mut h = [0u8; 4];
    s.read_exact(&mut h).await.map_err(io)?;
    if h[0] != 5 {
        return Err((0x01, format!("bad request version {}", h[0])));
    }
    let target = match h[3] {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await.map_err(io)?;
            (std::net::Ipv4Addr::from(a).to_string(), true)
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await.map_err(io)?;
            let v6 = std::net::Ipv6Addr::from(a);
            (v6.to_ipv4_mapped().map(|v| v.to_string()).unwrap_or_else(|| v6.to_string()), true)
        }
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await.map_err(io)?;
            let mut n = vec![0u8; l[0] as usize];
            s.read_exact(&mut n).await.map_err(io)?;
            let name = String::from_utf8_lossy(&n).trim_end_matches('.').to_ascii_lowercase();
            let lit = name.parse::<IpAddr>().is_ok();
            (name, lit)
        }
        t => return Err((0x08, format!("address type {t} is not supported"))),
    };
    let mut p = [0u8; 2];
    s.read_exact(&mut p).await.map_err(io)?;
    if h[1] != 0x01 {
        return Err((0x07, format!("only CONNECT is supported (command {})", h[1])));
    }
    Ok(SocksTarget { host: target.0, port: u16::from_be_bytes(p), ip_literal: target.1 })
}

async fn socks_reply<S: AsyncWrite + Unpin>(s: &mut S, code: u8) {
    let _ = s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await;
}

/// Serve one SOCKS5 connection from the guest (guest 127.0.0.1:7803 → vsock → here).
pub async fn serve_socks<S: AsyncRead + AsyncWrite + Unpin>(k: Arc<Kernel>, mut s: S) {
    let t0 = Instant::now();
    let Ok(Ok(actor)) = tokio::time::timeout(Duration::from_secs(30), socks_hello(&mut s)).await else {
        return; // probes and idle connections are not logged
    };
    let target = match tokio::time::timeout(Duration::from_secs(30), socks_request(&mut s)).await {
        Ok(Ok(t)) => t,
        Ok(Err((code, e))) => {
            socks_reply(&mut s, code).await;
            k.research.log("kernel", Kind::Egress, json!({ "method": "SOCKS5", "actor": actor, "decision": "invalid", "error": e }));
            return;
        }
        Err(_) => return,
    };
    let log = |k: &Kernel, decision: &str, reason: Reason, up: u64, down: u64, err: Option<String>| {
        k.research.log(
            "kernel",
            Kind::Egress,
            json!({
                "host": target.host, "port": target.port, "method": "SOCKS5", "ip_literal": target.ip_literal, "actor": actor,
                "decision": decision, "reason": reason.as_str(), "bytes_up": up, "bytes_down": down,
                "ms": t0.elapsed().as_millis() as u64, "error": err
            }),
        );
    };
    let (allowed, reason) = decide(&k, &target.host, target.port, &actor).await;
    if !allowed {
        socks_reply(&mut s, 0x02).await;
        log(&k, "deny", reason, 0, 0, None);
        return;
    }
    let mut up = match connect_upstream(&target.host, target.port).await {
        Ok(t) => t,
        Err(e) => {
            socks_reply(&mut s, e.socks()).await;
            if matches!(e, UpErr::Local) {
                log(&k, "deny", Reason::Local, 0, 0, None);
            } else {
                log(&k, "allow", reason, 0, 0, Some(e.log()));
            }
            return;
        }
    };
    socks_reply(&mut s, 0x00).await;
    let (a, b, err) = relay(&mut s, &mut up).await;
    log(&k, "allow", reason, a, b, err);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        assert!(default_allowed("registry.npmjs.org"));
        assert!(default_allowed("cdn-lfs.huggingface.co"));
        assert!(default_allowed("cas-bridge.xethub.hf.co"));
        assert!(default_allowed("ab.chatgpt.com"));
        assert!(!default_allowed("hf.co.evil.com"));
        assert!(default_allowed("github.com") && default_allowed("deb.debian.org"));
        assert!(!default_allowed("api.github.com") && !default_allowed("dl.google.com"));
        assert!(!default_allowed("xhuggingface.co"));
    }

    #[test]
    fn heads() {
        let h = parse_head("CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Authorization: Basic YWdlbnQ6\r\n\r\n").unwrap();
        assert_eq!((h.method.as_str(), h.host.as_str(), h.port, h.actor.as_str()), ("CONNECT", "github.com", 443, "agent"));
        let h = parse_head("GET http://Example.com/x?y=1 HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!((h.host.as_str(), h.port, h.path.as_str(), h.actor.as_str()), ("example.com", 80, "/x?y=1", "guest"));
        let h = parse_head("CONNECT [::1]:8080 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((h.host.as_str(), h.port), ("::1", 8080));
        assert!(parse_head("GET /relative HTTP/1.1\r\n\r\n").is_err());
        assert!(local_ip(&"127.0.0.1".parse().unwrap()) && local_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        assert!(!local_ip(&"140.82.112.3".parse().unwrap()));
    }

    #[test]
    fn card_texts() {
        assert_eq!(card_text("Telegram", "149.154.167.50", 443).0, "Telegram wants to reach the IP address 149.154.167.50:443");
        assert_eq!(card_text("Mail", "imap.gmail.com", 993).0, "Mail wants to reach imap.gmail.com:993");
        assert_eq!(card_text("Mail", "api.github.com", 443).0, "Mail wants to reach api.github.com");
        assert!(card_text("X", "2001:db8::1", 80).0.ends_with("[2001:db8::1]:80"));
    }

    #[tokio::test]
    async fn socks_parsing() {
        // user/pass auth names the actor; CONNECT to a domain (socks5h)
        let (mut c, mut srv) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            let a = socks_hello(&mut srv).await.unwrap();
            (a, socks_request(&mut srv).await.unwrap())
        });
        let mut req = vec![5, 2, 0, 2, 1, 11];
        req.extend_from_slice(b"module.mail");
        req.extend_from_slice(&[7]);
        req.extend_from_slice(b"openapp");
        req.extend_from_slice(&[5, 1, 0, 3, 14]);
        req.extend_from_slice(b"imap.gmail.com");
        req.extend_from_slice(&993u16.to_be_bytes());
        c.write_all(&req).await.unwrap();
        let mut ack = [0u8; 4];
        c.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack, [5, 2, 1, 0]);
        let (actor, t) = task.await.unwrap();
        assert_eq!(actor, "module.mail");
        assert_eq!(t, SocksTarget { host: "imap.gmail.com".into(), port: 993, ip_literal: false });
        // no auth + IPv4 literal
        let (mut c, mut srv) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            let a = socks_hello(&mut srv).await.unwrap();
            (a, socks_request(&mut srv).await.unwrap())
        });
        c.write_all(&[5, 1, 0, 5, 1, 0, 1, 149, 154, 167, 50, 1, 187]).await.unwrap();
        let (actor, t) = task.await.unwrap();
        assert_eq!(actor, "guest");
        assert_eq!(t, SocksTarget { host: "149.154.167.50".into(), port: 443, ip_literal: true });
        // BIND is refused with "command not supported"
        let (mut c, mut srv) = tokio::io::duplex(1024);
        c.write_all(&[5, 2, 0, 1, 1, 1, 1, 1, 0, 80]).await.unwrap();
        assert_eq!(socks_request(&mut srv).await.unwrap_err().0, 0x07);
    }
}
