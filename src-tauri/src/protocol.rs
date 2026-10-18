//! `oamod://<module>/<path>` handler: serves module files to sandboxed iframes (contract §4.2).

use std::path::Path;

pub struct ProtoResponse {
    pub status: u16,
    pub mime: &'static str,
    pub body: Vec<u8>,
}

impl ProtoResponse {
    fn text(status: u16, msg: impl Into<String>) -> Self {
        Self { status, mime: "text/plain; charset=utf-8", body: msg.into().into_bytes() }
    }
}

/// `window.openapp` bridge, served at `oamod://<module>/_openapp/bridge.js`.
pub const BRIDGE_JS: &str = r#"(function () {
  "use strict";
  if (window.openapp) return;
  var parentWin = window.parent;
  var seq = 0;
  var pending = {};
  var themeCbs = [];
  var lastVars = null;

  function post(msg) {
    try { parentWin.postMessage(msg, "*"); } catch (e) { /* detached */ }
  }
  function errText(e) {
    if (!e) return String(e);
    if (typeof e === "string") return e;
    return e.message ? String(e.message) : (function () { try { return JSON.stringify(e); } catch (_) { return String(e); } })();
  }
  function report(e) {
    post({ oa: "error", message: errText(e).slice(0, 2000) });
  }
  function request(kind, payload) {
    return new Promise(function (resolve, reject) {
      var id = "m" + (++seq) + "_" + Date.now().toString(36);
      var timer = setTimeout(function () {
        if (pending[id]) { delete pending[id]; reject(new Error("openapp." + kind + " timed out")); }
      }, 130000);
      pending[id] = { resolve: resolve, reject: reject, timer: timer };
      var msg = { oa: kind, id: id };
      for (var k in payload) if (Object.prototype.hasOwnProperty.call(payload, k)) msg[k] = payload[k];
      post(msg);
    });
  }
  function applyVars(vars) {
    var root = document.documentElement;
    if (!root || !vars) return;
    Object.keys(vars).forEach(function (k) {
      var v = vars[k];
      if (v === null || v === undefined) return;
      var name = k.indexOf("--") === 0 ? k : "--oa-" + k;
      if (typeof v === "number" && /radius$/.test(name)) v = v + "px";
      root.style.setProperty(name, String(v));
    });
  }

  window.addEventListener("message", function (ev) {
    if (ev.source !== parentWin) return;
    var m = ev.data;
    if (!m || typeof m !== "object" || typeof m.oa !== "string") return;
    if (m.oa === "result") {
      var p = pending[m.id];
      if (!p) return;
      delete pending[m.id];
      clearTimeout(p.timer);
      if (m.ok) p.resolve(m.result);
      else p.reject(new Error(errText(m.error)));
    } else if (m.oa === "channel") {
      // §21 G1 live channel message
      var list = chanSubs[m.topic];
      var data = m.b64 !== undefined && m.b64 !== null ? fromB64(m.b64) : m.data;
      chanLast[m.topic] = [data, m];
      if (list) list.slice().forEach(function (cb) { try { cb(data, m); } catch (e) { report(e); } });
    } else if (m.oa === "drop") {
      // §21 G4 files dropped on this view (already copied / granted by the kernel)
      dropCbs.slice().forEach(function (cb) { try { cb(m.files || [], m); } catch (e) { report(e); } });
    } else if (m.oa === "theme") {
      lastVars = m.vars || {};
      applyVars(lastVars);
      themeCbs.slice().forEach(function (cb) { try { cb(lastVars); } catch (e) { report(e); } });
    }
  });

  window.addEventListener("error", function (ev) {
    var where = ev.filename ? " (" + ev.filename + ":" + ev.lineno + ":" + ev.colno + ")" : "";
    post({ oa: "error", message: String(ev.message || (ev.error && ev.error.message) || "Script error") + where });
  });
  window.addEventListener("unhandledrejection", function (ev) {
    post({ oa: "error", message: "Unhandled promise rejection: " + errText(ev.reason) });
  });

  // §21 G4 drag and drop + shortcuts: drop callbacks; key presses outside text fields go to the shell
  // (app-wide shortcuts match there, e.g. Space = play in a DAW view)
  var dropCbs = [];
  window.addEventListener("keydown", function (e) {
    var t = e.target, tag = t && t.tagName;
    var typing = tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || (t && t.isContentEditable);
    if (typing && !e.metaKey && !e.ctrlKey) return;
    post({ oa: "key", code: e.code, meta: e.metaKey, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, typing: !!typing, repeat: e.repeat });
  }, true);

  // §21 G1 live channels: topic → callbacks; the last message per topic for late local subscribers
  var chanSubs = {};
  var chanLast = {};
  function fromB64(s) {
    var bin = atob(s), out = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }
  function toB64(d) {
    var u = d instanceof ArrayBuffer ? new Uint8Array(d) : new Uint8Array(d.buffer, d.byteOffset, d.byteLength);
    var s = "";
    for (var i = 0; i < u.length; i += 0x8000) s += String.fromCharCode.apply(null, u.subarray(i, i + 0x8000));
    return btoa(s);
  }

  // §21 G4 microphone / camera: consent card first (media.request, once per module and kind), then the native
  // getUserMedia; the kernel's capture gate allows this view's origin only with the grant.
  var md = navigator.mediaDevices;
  var nativeGUM = md && md.getUserMedia ? md.getUserMedia.bind(md) : null;
  function mediaConsent(c) {
    var want = { audio: !!(c && c.audio), video: !!(c && c.video) };
    if (!want.audio && !want.video) return Promise.resolve(want);
    return request("host", { action: "media.request", args: want });
  }
  function requestMedia(c) {
    c = c || { audio: true };
    if (!nativeGUM) return Promise.reject(new Error("getUserMedia is not available in this view"));
    return mediaConsent(c).then(function () { return nativeGUM(c); });
  }
  if (nativeGUM) {
    try { md.getUserMedia = requestMedia; } catch (e) { /* read-only: openapp.requestMedia still works */ }
  }

  window.openapp = Object.freeze({
    /** requestMedia({ audio?, video? }) → MediaStream after the module's consent card (Allow is remembered). */
    requestMedia: requestMedia,
    call: function (tool, args) { return request("call", { tool: String(tool), args: args || {} }); },
    host: function (action, args) { return request("host", { action: String(action), args: args || {} }); },
    /** subscribe(topic, (data, msg) => …) → unsubscribe. Binary messages arrive as Uint8Array. */
    subscribe: function (topic, cb) {
      topic = String(topic);
      if (typeof cb !== "function") return function () {};
      var list = chanSubs[topic];
      if (!list) {
        list = chanSubs[topic] = [];
        request("subscribe", { topic: topic }).catch(function (e) { delete chanSubs[topic]; report("openapp.subscribe(\"" + topic + "\"): " + errText(e)); });
      } else if (chanLast[topic]) {
        try { cb(chanLast[topic][0], chanLast[topic][1]); } catch (e) { report(e); }
      }
      list.push(cb);
      return function () {
        var l = chanSubs[topic];
        if (!l) return;
        var i = l.indexOf(cb);
        if (i >= 0) l.splice(i, 1);
        if (!l.length) { delete chanSubs[topic]; delete chanLast[topic]; post({ oa: "unsubscribe", topic: topic }); }
      };
    },
    /** publish(topic, data) on this module's topics; ArrayBuffer / typed arrays are sent as bytes. */
    publish: function (topic, data) {
      var p = { topic: String(topic) };
      if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) p.b64 = toB64(data);
      else p.data = data === undefined ? null : data;
      return request("publish", p);
    },
    /** onDrop((files) => …) → unsubscribe. files: [{ path, name, size, mime }] (paths work in the VM and host services). */
    onDrop: function (cb) {
      if (typeof cb !== "function") return function () {};
      dropCbs.push(cb);
      post({ oa: "dropzone", on: true });
      return function () { var i = dropCbs.indexOf(cb); if (i >= 0) dropCbs.splice(i, 1); if (!dropCbs.length) post({ oa: "dropzone", on: false }); };
    },
    /** dragOut({ path }) — call from a mousedown/dragstart handler: drags the file out of OpenApp (Finder, Mail, …). */
    dragOut: function (opts) { return request("host", { action: "files.drag_out", args: { path: String((opts && opts.path) || opts || "") } }); },
    onTheme: function (cb) {
      if (typeof cb !== "function") return function () {};
      themeCbs.push(cb);
      if (lastVars) { try { cb(lastVars); } catch (e) { report(e); } }
      return function () { var i = themeCbs.indexOf(cb); if (i >= 0) themeCbs.splice(i, 1); };
    }
  });

  // Base styles with zero specificity so module CSS always wins.
  try {
    var style = document.createElement("style");
    style.textContent = ":where(html,body){background:transparent}:where(body){margin:0;color:var(--oa-text);font-family:var(--oa-font)}";
    var me = document.currentScript;
    if (me && me.parentNode) me.parentNode.insertBefore(style, me.nextSibling);
    else (document.head || document.documentElement).appendChild(style);
  } catch (e) { /* ignore */ }

  // Content height reporting. Measures the content itself (not the viewport), so views can
  // shrink: union of body contents (Range) and of in-flow children incl. margins, plus the
  // body's own bottom padding/border/margin.
  var lastH = -1;
  var scheduled = false;
  function px(v) { var n = parseFloat(v); return isFinite(n) ? n : 0; }
  function measure() {
    scheduled = false;
    var b = document.body;
    if (!b) return;
    var sy = window.scrollY || window.pageYOffset || 0;
    var bottom = 0;
    try {
      var range = document.createRange();
      range.selectNodeContents(b);
      var rr = range.getBoundingClientRect();
      if (rr.height > 0) bottom = rr.bottom + sy;
    } catch (e) { /* ignore */ }
    for (var i = 0; i < b.children.length; i++) {
      var c = b.children[i];
      var cs = getComputedStyle(c);
      if (cs.position === "fixed" || cs.display === "none" || c.tagName === "SCRIPT" || c.tagName === "STYLE") continue;
      var r = c.getBoundingClientRect();
      bottom = Math.max(bottom, r.bottom + sy + px(cs.marginBottom));
    }
    var bs = getComputedStyle(b);
    var h = Math.ceil(bottom + px(bs.paddingBottom) + px(bs.borderBottomWidth) + px(bs.marginBottom));
    if (h !== lastH) { lastH = h; post({ oa: "resize", height: h }); }
  }
  function schedule() {
    if (scheduled) return;
    scheduled = true;
    (window.requestAnimationFrame || setTimeout)(measure);
  }
  function start() {
    if (typeof ResizeObserver === "function") {
      var ro = new ResizeObserver(schedule);
      ro.observe(document.documentElement);
      if (document.body) {
        ro.observe(document.body);
        for (var i = 0; i < document.body.children.length; i++) ro.observe(document.body.children[i]);
      }
    }
    if (typeof MutationObserver === "function" && document.body) {
      new MutationObserver(schedule).observe(document.body, { childList: true, subtree: true, attributes: true, characterData: true });
    }
    window.addEventListener("load", schedule);
    schedule();
    post({ oa: "ready" });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();
})();
"#;

pub fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" | "md" | "ts" | "tsx" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Split a request URI into (module, path). Accepts `oamod://<module>/<path>`
/// and the Windows/Android form `http(s)://oamod.<module>/<path>`.
pub fn parse_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.split_once("://").map(|(_, r)| r)?;
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.strip_prefix("oamod.").unwrap_or(host).to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or("").to_string();
    Some((host, percent_decode(path)?))
}

/// Inject the bridge as the first child of <head>.
pub fn inject_bridge(html: &str, module: &str) -> String {
    let tag = format!("<script src=\"oamod://{module}/_openapp/bridge.js\"></script>");
    let lower = html.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find("<head") {
        let at = search + pos;
        let next = lower.as_bytes().get(at + 5).copied();
        if matches!(next, Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r'))
            && let Some(end) = lower[at..].find('>') {
                let insert = at + end + 1;
                return format!("{}{}{}", &html[..insert], tag, &html[insert..]);
            }
        search = at + 5;
    }
    if let Some(pos) = lower.find("<html")
        && let Some(end) = lower[pos..].find('>') {
            let insert = pos + end + 1;
            return format!("{}<head>{}</head>{}", &html[..insert], tag, &html[insert..]);
        }
    format!("{tag}{html}")
}

pub fn handle(workspace: &Path, uri: &str) -> ProtoResponse {
    let Some((module, path)) = parse_uri(uri) else {
        return ProtoResponse::text(400, "bad oamod URL");
    };
    if !crate::modules::valid_module_id(&module) {
        return ProtoResponse::text(400, format!("bad module id \"{module}\""));
    }
    if path == "_openapp/bridge.js" {
        return ProtoResponse { status: 200, mime: "text/javascript; charset=utf-8", body: BRIDGE_JS.as_bytes().to_vec() };
    }
    let path = if path.is_empty() || path.ends_with('/') { format!("{path}index.html") } else { path };
    if !crate::modules::safe_rel(&path) || path.split('/').any(|seg| seg.starts_with('.')) {
        return ProtoResponse::text(403, "forbidden path");
    }
    let base = workspace.join("modules").join(&module);
    let Ok(base) = base.canonicalize() else {
        return ProtoResponse::text(404, format!("no module \"{module}\""));
    };
    let full = match base.join(&path).canonicalize() {
        Ok(p) => p,
        Err(_) => return ProtoResponse::text(404, format!("not found: {module}/{path}")),
    };
    if !full.starts_with(&base) || !full.is_file() {
        return ProtoResponse::text(403, "forbidden path");
    }
    let Ok(bytes) = std::fs::read(&full) else {
        return ProtoResponse::text(500, "read failed");
    };
    let mime = mime_for(&path);
    if mime.starts_with("text/html") {
        let html = String::from_utf8_lossy(&bytes);
        return ProtoResponse { status: 200, mime, body: inject_bridge(&html, &module).into_bytes() };
    }
    ProtoResponse { status: 200, mime, body: bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject() {
        let out = inject_bridge("<!doctype html><html><HEAD lang=x><title>x</title></head></html>", "m");
        assert!(out.contains("<HEAD lang=x><script src=\"oamod://m/_openapp/bridge.js\"></script><title>"));
        let out = inject_bridge("<html><header></header></html>", "m");
        assert!(out.starts_with("<html><head><script"));
        assert!(inject_bridge("<p>hi</p>", "m").starts_with("<script"));
    }

    #[test]
    fn traversal_blocked() {
        let ws = std::env::temp_dir().join(format!("oa-proto-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(ws.join("modules/m/views")).unwrap();
        std::fs::write(ws.join("modules/m/views/a.html"), "<html><head></head><body>x</body></html>").unwrap();
        std::fs::write(ws.join("secret.txt"), "s").unwrap();
        let r = handle(&ws, "oamod://m/views/a.html");
        assert_eq!(r.status, 200);
        assert!(String::from_utf8(r.body).unwrap().contains("bridge.js"));
        assert_eq!(handle(&ws, "oamod://m/../../secret.txt").status, 403);
        assert_eq!(handle(&ws, "oamod://m/%2e%2e/%2e%2e/secret.txt").status, 403);
        assert_eq!(handle(&ws, "oamod://m/_openapp/bridge.js").status, 200);
        assert_eq!(handle(&ws, "http://oamod.m/views/a.html").status, 200);
        std::fs::remove_dir_all(ws).ok();
    }
}
