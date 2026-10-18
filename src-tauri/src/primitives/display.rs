//! G3 virtual display (contract §21): manifest `"display": { command: [...], width?, height? }` → one
//! supervised service `display` in the app VM: Xvfb `:N`, the app (`DISPLAY=:N`), x11vnc (127.0.0.1:5900+N)
//! and websockify serving noVNC (/usr/share/novnc) and the VNC WebSocket on 127.0.0.1:6080+N (exposed
//! `view`). Views show it with host action `display.url` → a noVNC page on the kernel's local gateway
//! (HTTP + WebSocket; see ports.rs) for an iframe or web view, or `ws` for their own noVNC client.

use crate::util::MutexExt;
use super::services::{Expose, Health, PortSpec, Restart, ServiceSpec};
use super::*;

pub const VNC_BASE: u16 = 5900;
pub const WEB_BASE: u16 = 6080;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DisplaySpec {
    pub command: Vec<String>,
    pub width: u32,
    pub height: u32,
}

pub fn parse(v: Option<&Value>) -> Result<Option<DisplaySpec>, Vec<String>> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
    let mut p = Vec::new();
    let command = match v.get("command") {
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) => {
            a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect()
        }
        _ => {
            p.push("display.command must be a non-empty array of strings, e.g. [\"xterm\"] or [\"libreoffice\", \"--calc\"]".into());
            vec![]
        }
    };
    let dim = |key: &str, def: u32, p: &mut Vec<String>| match v.get(key) {
        None | Some(Value::Null) => def,
        Some(x) => match x.as_u64().filter(|n| (320..=7680).contains(n)) {
            Some(n) => n as u32,
            None => {
                p.push(format!("display.{key} must be a number of pixels (320–7680)"));
                def
            }
        },
    };
    let width = dim("width", 1280, &mut p);
    let height = dim("height", 800, &mut p);
    if p.is_empty() { Ok(Some(DisplaySpec { command, width, height })) } else { Err(p) }
}

/// Display number of a module (1, 2, …; stable while the kernel runs).
pub fn number(k: &Kernel, module: &str) -> u16 {
    let st = super::services::state(k);
    let mut d = st.displays.locked();
    if let Some(n) = d.get(module) {
        return *n;
    }
    let n = (1..).find(|n| !d.values().any(|v| v == n)).unwrap_or(1);
    d.insert(module.to_string(), n);
    n
}

pub fn ports(n: u16) -> (u16, u16) {
    (VNC_BASE + n, WEB_BASE + n)
}

const SCRIPT: &str = r#"set -u
n=$OPENAPP_DISPLAY_N; vnc=$OPENAPP_VNC_PORT; web=$OPENAPP_WEB_PORT
rm -f /tmp/.X$n-lock /tmp/.X11-unix/X$n
Xvfb :$n -screen 0 "${OPENAPP_DISPLAY_SIZE}x24" -nolisten tcp -ac &
for i in $(seq 200); do [ -S /tmp/.X11-unix/X$n ] && break; sleep 0.05; done
export DISPLAY=:$n
x11vnc -display :$n -rfbport $vnc -localhost -forever -shared -nopw -quiet -noshm -noxdamage >/dev/null 2>&1 &
websockify --web /usr/share/novnc 127.0.0.1:$web 127.0.0.1:$vnc 2>&1 | grep -v 'code 200\|code 101' &
if [ "${OPENAPP_QEMU:-}" = 1 ]; then q=$(command -v qemu-x86_64-static || command -v qemu-x86_64); "$q" "$@" & else "$@" & fi
wait -n
echo "display: a process exited; restarting the display" >&2
exit 1
"#;

/// The synthesized `display` service.
pub fn service_spec(k: &Kernel, module: &str, d: &DisplaySpec) -> ServiceSpec {
    let n = number(k, module);
    let (vnc, web) = ports(n);
    let amd64 = k
        .lock()
        .modules
        .get(module)
        .and_then(|m| m.manifest.as_ref().map(|mf| mf.raw.get("arch").and_then(Value::as_str).is_some_and(|a| a == "amd64" || a == "x86_64")))
        .unwrap_or(false);
    let mut command = vec!["bash".to_string(), "-c".to_string(), SCRIPT.to_string(), "display".to_string()];
    command.extend(d.command.iter().cloned());
    let mut env = BTreeMap::new();
    env.insert("OPENAPP_DISPLAY_N".into(), n.to_string());
    env.insert("OPENAPP_VNC_PORT".into(), vnc.to_string());
    env.insert("OPENAPP_WEB_PORT".into(), web.to_string());
    env.insert("OPENAPP_DISPLAY_SIZE".into(), format!("{}x{}", d.width, d.height));
    if amd64 {
        env.insert("OPENAPP_QEMU".into(), "1".into());
    }
    ServiceSpec {
        name: "display".into(),
        command,
        cwd: None,
        env,
        restart: Restart::Always,
        health: Some(Health::Tcp(web)),
        ports: vec![PortSpec { port: web, expose: Expose::View }],
        socks: false,
    }
}

async fn act_url(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let m = arg(&p, "module").or_else(|| c.module()).ok_or("\"module\" is required")?.to_string();
    super::services::allowed_module(&c, &m)?;
    let declared = super::services::state(&k).mods.locked().get(&m).is_some_and(|r| r.extras.display.is_some());
    if !declared {
        return Err(format!("module \"{m}\" has no \"display\" (declare \"display\": {{ \"command\": [\"xterm\"] }} in openapp.json and load it)"));
    }
    let n = number(&k, &m);
    let (vnc, web) = ports(n);
    let base = super::ports::gateway_base(&k, &m).await?;
    let ws_path = format!("{}/{m}/{web}/websockify", super::ports::token_for(&k, &m));
    let gw = base.trim_end_matches('/').to_string();
    Ok(json!({
        "page": format!("{gw}/{m}/{web}/vnc.html?autoconnect=1&reconnect=1&resize=scale&path={ws_path}"),
        "lite": format!("{gw}/{m}/{web}/vnc_lite.html?path={ws_path}&scale=1"),
        "ws": format!("{}/{m}/{web}/websockify", gw.replacen("http://", "ws://", 1)),
        "rfb_module": format!("{gw}/{m}/{web}/core/rfb.js"),
        "oasvc": super::ports::oasvc_url(&m, web, "/vnc.html"),
        "display": format!(":{n}"),
        "vnc_port": vnc,
        "web_port": web,
        "how": "show `page` in an iframe (html view) or a web view (web.navigate); or import RFB from `rfb_module` and `new RFB(el, ws)`",
    }))
}

pub fn primitive() -> Primitive {
    Primitive {
        name: "display",
        purpose: "Linux GUI apps in a virtual display (manifest \"display\" → Xvfb + x11vnc + noVNC over WebSocket in the app VM), shown in a view",
        example: "\"display\": { \"command\": [\"xterm\"], \"width\": 1280, \"height\": 800 } → html view: const d = await openapp.host(\"display.url\", {}); iframe.src = d.page",
        class: Class::Free,
        actions: vec![Action { name: "display.url", args: "{ module } → { page (noVNC, autoconnect), ws (VNC WebSocket), rfb_module, display, vnc_port, web_port }", handler: handler(act_url) }],
        endpoints: vec![Endpoint { method: "display.url", args: "{}", handler: handler(act_url) }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display() {
        let d = parse(Some(&json!({ "command": ["xterm"] }))).unwrap().unwrap();
        assert_eq!((d.width, d.height), (1280, 800));
        assert!(parse(Some(&json!({ "command": "xterm", "width": 10 }))).unwrap_err().len() == 2);
        assert!(parse(None).unwrap().is_none());
        assert_eq!(ports(1), (5901, 6081));
    }
}
