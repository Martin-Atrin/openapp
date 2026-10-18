//! G2 exposed ports (contract §21): VM service ports shown on the host.
//!
//! - `oasvc://<module>/<port>/<path>`: a Tauri custom protocol (registered in lib.rs) that proxies one
//!   HTTP request to guest `127.0.0.1:<port>` over the bridge (`socat` exec per request, `Connection:
//!   close`, chunked bodies decoded). Only ports a loaded module declares in `services[].ports` (or its
//!   `display`) are reachable. Redirects to the guest's own address are rewritten to oasvc://. Absolute
//!   paths (`/app.js`) are resolved through the Referer or, failing that, the module's only port.
//!   WKWebView custom schemes carry no WebSockets, so:
//! - Gateway: one kernel listener on host 127.0.0.1:<random>, URL `http://127.0.0.1:<g>/<token>/<module>/<port>/<path>`
//!   (token per module, a capability URL). The request line is rewritten and the connection is then
//!   passed through raw, so HTTP and WebSocket upgrades both work (noVNC, dev servers with HMR). Non-upgrade
//!   requests get `Connection: close` (each request carries its prefix). Host action `service.url`.
//! - `expose: "lan"`: after the user allows it (Granted, consent card, `grants.json` `lan.<module>:<port>`),
//!   a host listener on 0.0.0.0:<port> forwards to the guest port; Bonjour name via `dns-sd -R` (the module
//!   name, `_http._tcp`). Revoke with `grant.revoke { id: "lan:<module>:<port>" }` (applies on next start).

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

const HEAD_MAX: usize = 64 * 1024;
const BODY_MAX: usize = 64 * 1024 * 1024;
const REQ_TIMEOUT: Duration = Duration::from_secs(60);

pub fn oasvc_url(module: &str, port: u16, path: &str) -> String {
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    format!("oasvc://{module}/{port}{path}")
}

#[derive(Default)]
pub struct PortsState {
    tokens: Mutex<BTreeMap<String, String>>,
    gateway: tokio::sync::Mutex<Option<(u16, JoinHandle<()>)>>,
    lan: Mutex<BTreeMap<(String, u16), LanRun>>,
}

struct LanRun {
    status: String,
    tasks: Vec<JoinHandle<()>>,
    dns: Option<tokio::process::Child>,
}

impl Drop for LanRun {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Some(c) = self.dns.as_mut() {
            let _ = c.start_kill();
        }
    }
}

fn pstate(k: &Kernel) -> Arc<PortsState> {
    k.prims.get::<PortsState>()
}

pub fn token_for(k: &Kernel, module: &str) -> String {
    let st = pstate(k);
    let mut t = st.tokens.locked();
    t.entry(module.to_string()).or_insert_with(|| format!("g{}", uuid::Uuid::new_v4().simple())).clone()
}

fn module_of_token(k: &Kernel, token: &str) -> Option<String> {
    pstate(k).tokens.locked().iter().find(|(_, t)| t.as_str() == token).map(|(m, _)| m.clone())
}

fn declared(k: &Kernel, module: &str, port: u16) -> bool {
    super::services::module_ports(k, module).contains(&port)
}

// ---------- HTTP over a guest stream ----------

pub struct ProxyResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let i = b.windows(2).position(|w| w == b"\r\n").ok_or("bad chunked body")?;
        let line = std::str::from_utf8(&b[..i]).map_err(|_| "bad chunk size")?;
        let n = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| format!("bad chunk size {line:?}"))?;
        b = &b[i + 2..];
        if n == 0 {
            return Ok(out);
        }
        if b.len() < n {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&b[..n]);
        b = b.get(n + 2..).unwrap_or(&[]);
    }
}

/// Parse a full HTTP/1.1 response (connection closed by the server).
pub fn parse_response(raw: &[u8]) -> Result<ProxyResponse, String> {
    let end = find_head_end(raw).ok_or("no response head from the service")?;
    let head = String::from_utf8_lossy(&raw[..end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let status: u16 = first.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad status line {first:?}"))?;
    let mut headers = Vec::new();
    for l in lines {
        if let Some((n, v)) = l.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let rest = &raw[end + 4..];
    let h = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let body = if h("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        dechunk(rest)?
    } else if let Some(n) = h("content-length").and_then(|v| v.parse::<usize>().ok()) {
        rest[..n.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    headers.retain(|(n, _)| {
        let l = n.to_ascii_lowercase();
        !matches!(l.as_str(), "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "x-frame-options")
    });
    Ok(ProxyResponse { status, headers, body })
}

async fn read_all<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > max {
            return Err(format!("response larger than {} MB", max / 1024 / 1024));
        }
    }
}

/// One HTTP request to guest 127.0.0.1:`port` (of a declared module port).
pub async fn proxy_http(k: &Arc<Kernel>, module: &str, port: u16, method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<ProxyResponse, (u16, String)> {
    if !declared(k, module, port) {
        return Err((404, format!("module \"{module}\" does not expose port {port} (declare it in services[].ports and load the module)")));
    }
    let vm = k.vm().ok_or((503, "the app VM is not running".to_string()))?;
    let mut child = vm.connect_tcp(port).await.map_err(|e| (502, e))?;
    let kill = child.killer();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (n, v) in headers {
        let l = n.to_ascii_lowercase();
        if matches!(l.as_str(), "host" | "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "upgrade" | "proxy-connection") {
            continue;
        }
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut gin = child.take_stdin();
    let mut gout = child.take_stdout();
    let res = tokio::time::timeout(REQ_TIMEOUT, async {
        gin.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        gin.write_all(body).await.map_err(|e| e.to_string())?;
        gin.flush().await.map_err(|e| e.to_string())?;
        // keep stdin open: socat ends 0.5 s after its input closes
        read_all(&mut gout, BODY_MAX).await
    })
    .await;
    kill.kill();
    drop(gin);
    let raw = match res {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err((502, e)),
        Err(_) => return Err((504, format!("the service on port {port} did not answer within {} s", REQ_TIMEOUT.as_secs()))),
    };
    if raw.is_empty() {
        return Err((502, format!("nothing is listening on port {port} in the app VM (is the service running? see service.status)")));
    }
    let mut r = parse_response(&raw).map_err(|e| (502, e))?;
    for (n, v) in r.headers.iter_mut() {
        if n.eq_ignore_ascii_case("location") {
            for host in [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")] {
                if let Some(rest) = v.strip_prefix(&host) {
                    *v = oasvc_url(module, port, if rest.is_empty() { "/" } else { rest });
                }
            }
        }
    }
    Ok(r)
}

/// Split `oasvc://<module>/<port>/<path>` (or a referer-relative absolute path) into (module, port, path).
pub fn route(url: &str, referer: Option<&str>, only_port: &dyn Fn(&str) -> Option<u16>) -> Result<(String, u16, String), String> {
    let rest = url.strip_prefix("oasvc://").ok_or("not an oasvc:// URL")?;
    let (module, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if !crate::modules::valid_module_id(module) {
        return Err(format!("\"{module}\" is not a module id"));
    }
    let seg = tail.trim_start_matches('/').split(['/', '?']).next().unwrap_or("");
    if let Ok(port) = seg.parse::<u16>() {
        let path = &tail[1 + seg.len()..];
        let path = if path.is_empty() { "/".to_string() } else if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
        return Ok((module.to_string(), port, path));
    }
    // an absolute path inside a page: the referer's port, else the module's only port
    let from_ref = referer.and_then(|r| route(r, None, &|_| None).ok()).filter(|(m, _, _)| m == module).map(|(_, p, _)| p);
    let port = from_ref.or_else(|| only_port(module)).ok_or("oasvc URL must be oasvc://<module>/<port>/<path>")?;
    Ok((module.to_string(), port, tail.to_string()))
}

/// The oasvc:// protocol handler (also used by tests without a window).
pub async fn handle(k: &Arc<Kernel>, req: tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let url = req.uri().to_string();
    let referer = req.headers().get("referer").and_then(|v| v.to_str().ok()).map(String::from);
    let resp = |status: u16, msg: String| {
        tauri::http::Response::builder()
            .status(status)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Access-Control-Allow-Origin", "*")
            .body(msg.into_bytes())
            .unwrap_or_default()
    };
    let only_port = |m: &str| {
        let ps = super::services::module_ports(k, m);
        if ps.len() == 1 { Some(ps[0]) } else { None }
    };
    let (module, port, path) = match route(&url, referer.as_deref(), &only_port) {
        Ok(r) => r,
        Err(e) => return resp(400, e),
    };
    let headers: Vec<(String, String)> = req.headers().iter().filter_map(|(n, v)| v.to_str().ok().map(|v| (n.to_string(), v.to_string()))).collect();
    let method = req.method().to_string();
    match proxy_http(k, &module, port, &method, &path, &headers, req.body()).await {
        Ok(r) => {
            let mut b = tauri::http::Response::builder().status(r.status).header("Access-Control-Allow-Origin", "*").header("Cache-Control", "no-store");
            for (n, v) in &r.headers {
                if n.eq_ignore_ascii_case("access-control-allow-origin") || n.eq_ignore_ascii_case("cache-control") {
                    continue;
                }
                b = b.header(n.as_str(), v.as_str());
            }
            b.body(r.body).unwrap_or_default()
        }
        Err((status, msg)) => resp(status, msg),
    }
}

/// Tauri glue: `.register_asynchronous_uri_scheme_protocol("oasvc", primitives::ports::tauri_protocol)`.
pub fn tauri_protocol<R: tauri::Runtime>(ctx: tauri::UriSchemeContext<'_, R>, req: tauri::http::Request<Vec<u8>>, responder: tauri::UriSchemeResponder) {
    use tauri::Manager;
    let k = ctx.app_handle().try_state::<Arc<Kernel>>().map(|s| s.inner().clone());
    tauri::async_runtime::spawn(async move {
        let r = match k {
            Some(k) => handle(&k, req).await,
            None => tauri::http::Response::builder().status(503).body(b"kernel not ready".to_vec()).unwrap_or_default(),
        };
        responder.respond(r);
    });
}

// ---------- gateway (HTTP + WebSocket passthrough) ----------

/// `http://127.0.0.1:<g>/<token>` for a module (starts the gateway on first use).
pub async fn gateway_base(k: &Arc<Kernel>, module: &str) -> Result<String, String> {
    let st = pstate(k);
    let mut g = st.gateway.lock().await;
    if g.as_ref().is_none_or(|(_, t)| t.is_finished()) {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("gateway listener: {e}"))?;
        let port = l.local_addr().map_err(|e| e.to_string())?.port();
        let weak = Arc::downgrade(k);
        let t = tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let Some(k) = weak.upgrade() else { break };
                tokio::spawn(async move { gateway_conn(k, s).await });
            }
        });
        log::info!("oasvc gateway on 127.0.0.1:{port}");
        *g = Some((port, t));
    }
    let port = g.as_ref().map(|(p, _)| *p).unwrap_or(0);
    Ok(format!("http://127.0.0.1:{port}/{}", token_for(k, module)))
}

async fn reply<S: AsyncWrite + Unpin>(s: &mut S, status: &str, msg: &str) {
    let r = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}", msg.len());
    let _ = s.write_all(r.as_bytes()).await;
    let _ = s.shutdown().await;
}

/// Rewrite a gateway request head: (module, port, new head) or an error status.
pub fn rewrite_head(head: &str, module_of: impl Fn(&str) -> Option<String>) -> Result<(String, u16, String), (&'static str, String)> {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(("400 Bad Request", "bad request line".into()));
    };
    let mut seg = target.trim_start_matches('/').splitn(4, '/');
    let (token, module, port) = (seg.next().unwrap_or(""), seg.next().unwrap_or(""), seg.next().unwrap_or(""));
    let rest = seg.next().unwrap_or("");
    let port: u16 = port.split('?').next().unwrap_or("").parse().map_err(|_| ("404 Not Found", "path must be /<token>/<module>/<port>/…".to_string()))?;
    match module_of(token) {
        Some(m) if m == module => {}
        _ => return Err(("403 Forbidden", "unknown gateway token for this module".into())),
    }
    let upgrade = head.lines().any(|l| l.to_ascii_lowercase().starts_with("upgrade:"));
    let path = if rest.is_empty() { "/".to_string() } else { format!("/{rest}") };
    let mut out = format!("{method} {path} {version}\r\n");
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("host:") || (!upgrade && (lower.starts_with("connection:") || lower.starts_with("keep-alive:"))) {
            continue;
        }
        out.push_str(l);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    if !upgrade {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    Ok((module.to_string(), port, out))
}

async fn gateway_conn(k: Arc<Kernel>, mut s: tokio::net::TcpStream) {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let end = loop {
        let n = match tokio::time::timeout(Duration::from_secs(20), s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_head_end(&buf) {
            break i;
        }
        if buf.len() > HEAD_MAX {
            return reply(&mut s, "431 Request Header Fields Too Large", "head too large").await;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end + 4]).to_string();
    let rest = buf[end + 4..].to_vec();
    let (module, port, new_head) = match rewrite_head(&head, |t| module_of_token(&k, t)) {
        Ok(r) => r,
        Err((st, msg)) => return reply(&mut s, st, &msg).await,
    };
    if !declared(&k, &module, port) {
        return reply(&mut s, "404 Not Found", &format!("module {module} does not expose port {port}")).await;
    }
    let Some(vm) = k.vm() else { return reply(&mut s, "503 Service Unavailable", "the app VM is not running").await };
    let Ok(mut child) = vm.connect_tcp(port).await else { return reply(&mut s, "502 Bad Gateway", "bridge connect failed").await };
    let kill = child.killer();
    let mut gin = child.take_stdin();
    let mut gout = child.take_stdout();
    if gin.write_all(new_head.as_bytes()).await.is_err() || gin.write_all(&rest).await.is_err() {
        kill.kill();
        return;
    }
    let (mut tr, mut tw) = s.into_split();
    let up = async move {
        let _ = tokio::io::copy(&mut tr, &mut gin).await;
        // the client closed its side: give the response a moment, then end (socat drains)
        let _ = gin.shutdown().await;
    };
    let down = async move {
        let _ = tokio::io::copy(&mut gout, &mut tw).await;
        let _ = tw.shutdown().await;
    };
    tokio::join!(up, down);
    kill.kill();
}

/// Host action / endpoint `service.url { module, port, path? }`.
pub async fn act_url(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = arg(&p, "module").or_else(|| c.module()).ok_or("\"module\" is required")?.to_string();
    super::services::allowed_module(&c, &m)?;
    let port = p.get("port").and_then(Value::as_u64).filter(|p| (1..=65535).contains(p)).ok_or("\"port\" is required")? as u16;
    if !declared(&k, &m, port) {
        return Err(format!("module \"{m}\" does not expose port {port} (declare it in services[].ports and load the module)"));
    }
    let path = arg(&p, "path").unwrap_or("/");
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let base = gateway_base(&k, &m).await?;
    let http = format!("{base}/{m}/{port}{path}");
    Ok(json!({ "url": oasvc_url(&m, port, &path), "http": http, "ws": http.replacen("http://", "ws://", 1), "lan": lan_status(&k, &m, port) }))
}

// ---------- LAN ----------

pub fn lan_status(k: &Kernel, module: &str, port: u16) -> Value {
    let st = pstate(k);
    let lan = st.lan.locked();
    match lan.get(&(module.to_string(), port)) {
        Some(r) => json!(r.status),
        None => Value::Null,
    }
}

fn set_lan(k: &Kernel, module: &str, port: u16, status: &str) {
    let st = pstate(k);
    if let Some(r) = st.lan.locked().get_mut(&(module.to_string(), port)) {
        r.status = status.to_string();
    }
    k.emit_snapshot();
}

/// Consent card (Granted): Allow / Deny, remembered for the workspace (grant `lan:<module>:<port>`).
async fn lan_consent(k: &Arc<Kernel>, module: &str, service: &str, port: u16) -> bool {
    let name = k.lock().modules.get(module).map(|m| m.name.clone()).unwrap_or_else(|| module.to_string());
    let req = crate::consent::ConsentReq::new(
        "lan",
        format!("{name} wants to share \"{service}\" on your local network (port {port})"),
        "Other devices on this network (phones, other computers) will be able to connect to it. You can revoke it in the grants list.",
    )
    .options(
        json!([
            { "optionId": "allow_workspace", "name": "Allow on my network", "kind": "allow_always" },
            { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
        ]),
        &["allow_workspace"],
    )
    .info(json!({ "module": module, "port": port }))
    .timeout(Duration::from_secs(3600));
    crate::consent::granted(k, &format!("{module}:{port}"), req, true, Value::Null).await.allowed
}

pub fn lan_start(k: &Arc<Kernel>, module: &str, service: &str, port: u16) {
    let key = (module.to_string(), port);
    let st = pstate(k);
    {
        let mut lan = st.lan.locked();
        lan.remove(&key);
        lan.insert(key.clone(), LanRun { status: "asking".into(), tasks: vec![], dns: None });
    }
    let (k2, module, service) = (k.clone(), module.to_string(), service.to_string());
    let t = tokio::spawn(async move {
        if !lan_consent(&k2, &module, &service, port).await {
            set_lan(&k2, &module, port, "denied");
            return;
        }
        let l = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                set_lan(&k2, &module, port, &format!("error: host port {port} is busy ({e})"));
                return;
            }
        };
        let dns = if cfg!(target_os = "macos") {
            let label = k2.lock().modules.get(&module).map(|m| m.name.clone()).unwrap_or_else(|| module.clone());
            tokio::process::Command::new("/usr/bin/dns-sd")
                .args(["-R", &label, "_http._tcp", "local", &port.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .ok()
        } else {
            None
        };
        let host = hostname();
        {
            let st = pstate(&k2);
            if let Some(r) = st.lan.locked().get_mut(&(module.clone(), port)) {
                r.dns = dns;
            }
        }
        set_lan(&k2, &module, port, &format!("on: http://{host}:{port}/"));
        k2.research.log(&format!("module:{module}"), Kind::LanExpose, json!({ "port": port, "service": service }));
        let weak = Arc::downgrade(&k2);
        drop(k2);
        while let Ok((tcp, peer)) = l.accept().await {
            let Some(k) = weak.upgrade() else { break };
            let Some(vm) = k.vm() else { continue };
            k.research.log(&format!("module:{module}"), Kind::LanConnection, json!({ "port": port, "peer": peer.ip().to_string() }));
            tokio::spawn(async move {
                let Ok(mut child) = vm.connect_tcp(port).await else { return };
                let kill = child.killer();
                let (mut tr, mut tw) = tcp.into_split();
                let mut gin = child.take_stdin();
                let mut gout = child.take_stdout();
                let up = async move {
                    let _ = tokio::io::copy(&mut tr, &mut gin).await;
                    let _ = gin.shutdown().await;
                };
                let down = async move {
                    let _ = tokio::io::copy(&mut gout, &mut tw).await;
                    let _ = tw.shutdown().await;
                };
                tokio::join!(up, down);
                kill.kill();
            });
        }
    });
    if let Some(r) = st.lan.locked().get_mut(&key) {
        r.tasks.push(t);
    }
}

pub fn lan_stop(k: &Kernel, module: &str) {
    let st = pstate(k);
    st.lan.locked().retain(|(m, _), _| m != module);
}

fn hostname() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| format!("{}.local", s.trim()))
        .filter(|s| s.len() > 6)
        .unwrap_or_else(|| "this-mac.local".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let none: &dyn Fn(&str) -> Option<u16> = &|_| None;
        assert_eq!(route("oasvc://web/8080/a/b?x=1", None, none).unwrap(), ("web".into(), 8080, "/a/b?x=1".into()));
        assert_eq!(route("oasvc://web/8080", None, none).unwrap(), ("web".into(), 8080, "/".into()));
        assert_eq!(route("oasvc://web/8080?q", None, none).unwrap(), ("web".into(), 8080, "/?q".into()));
        assert_eq!(route("oasvc://web/static/app.js", Some("oasvc://web/3000/index.html"), none).unwrap(), ("web".into(), 3000, "/static/app.js".into()));
        assert_eq!(route("oasvc://web/static/app.js", None, &|_| Some(5173)).unwrap().1, 5173);
        assert!(route("oasvc://Bad_Id/1/", None, none).is_err());
        assert!(route("oasvc://web/static", None, none).is_err());
    }

    #[test]
    fn responses() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nX-Frame-Options: DENY\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"hello world".as_slice()));
        assert!(r.headers.iter().all(|(n, _)| !n.eq_ignore_ascii_case("x-frame-options") && !n.eq_ignore_ascii_case("transfer-encoding")));
        let r = parse_response(b"HTTP/1.0 404 Not Found\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (404, b"abc".as_slice()));
    }

    #[test]
    fn gateway_heads() {
        let of = |t: &str| (t == "gT").then(|| "vnc".to_string());
        let (m, p, h) = rewrite_head("GET /gT/vnc/6081/websockify HTTP/1.1\r\nHost: 127.0.0.1:5555\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n", of).unwrap();
        assert_eq!((m.as_str(), p), ("vnc", 6081));
        assert!(h.starts_with("GET /websockify HTTP/1.1\r\n") && h.contains("Connection: Upgrade") && h.contains("Host: 127.0.0.1:6081"));
        let (_, _, h) = rewrite_head("GET /gT/vnc/6081/app/ui.js HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n", of).unwrap();
        assert!(h.contains("Connection: close") && !h.contains("keep-alive"));
        assert!(rewrite_head("GET /bad/vnc/6081/ HTTP/1.1\r\n\r\n", of).is_err());
        assert!(rewrite_head("GET /gT/other/6081/ HTTP/1.1\r\n\r\n", of).is_err());
    }
}
