//! §20.3 guided consoles: agent control of web views (`web.read`, `web.click`, `web.fill`, `web.highlight`;
//! capability `web.control:<site>`, granted per site with a kernel card; no raw eval), the guide strip
//! (`guide.set` / `guide.clear` / `guide.done`), and download capture (OAuth client JSON → Keychain card;
//! other files → `modules/<id>/data/downloads/`).

use crate::research::Kind;
use crate::util::MutexExt;
use crate::kernel::{HostError, Kernel};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Clicks on elements named like this are left to the user (highlight + wait for "Done").
pub const GUARD: &str = r"\b(accept|agree|allow|authori[sz]e|grant|consent|pay|purchase|buy|subscribe|billing|upgrade|delete|publish|create|enable|confirm)\b";

/// The page-side helper (plain browser JS, evaluated per call; returns a JSON-serializable object).
/// Elements get stable refs in `data-oa-ref` (kept across reads of the same document).
pub const PAGE_LIB: &str = r##"function (op, a) {
  var W = window, D = document;
  if (!W.__oaSeq) W.__oaSeq = 0;
  var SEL = 'a[href],button,input,select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=radio],[role=tab],[role=menuitem],[role=menuitemcheckbox],[role=option],[role=switch],[role=combobox],[role=textbox],[role=searchbox],[contenteditable=""],[contenteditable="true"],[onclick],[tabindex]:not([tabindex="-1"])';
  function clean(s, n) { s = (s || '').replace(/\s+/g, ' ').trim(); return n && s.length > n ? s.slice(0, n - 1) + '…' : s; }
  function vis(el) {
    var r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) return false;
    var s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none' && s.opacity !== '0';
  }
  function roleOf(el) {
    var r = el.getAttribute('role'); if (r) return r;
    var t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'button' || t === 'summary') return 'button';
    if (t === 'select') return 'combobox';
    if (t === 'textarea') return 'textbox';
    if (t === 'input') {
      var ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'checkbox' || ty === 'radio') return ty;
      if (ty === 'submit' || ty === 'button' || ty === 'reset' || ty === 'image') return 'button';
      if (ty === 'search') return 'searchbox';
      if (ty === 'hidden') return null;
      if (ty === 'file') return 'file';
      return 'textbox';
    }
    if (el.isContentEditable) return 'textbox';
    return 'button';
  }
  function nameOf(el) {
    var v = el.getAttribute('aria-label'); if (v && clean(v)) return clean(v, 120);
    var lb = el.getAttribute('aria-labelledby');
    if (lb) {
      var t = lb.split(/\s+/).map(function (id) { var e = D.getElementById(id); return e ? (e.innerText || e.textContent) : ''; }).join(' ');
      if (clean(t)) return clean(t, 120);
    }
    if (el.labels && el.labels.length) {
      var l = clean(Array.prototype.map.call(el.labels, function (x) { return x.innerText || x.textContent; }).join(' '), 120);
      if (l) return l;
    }
    var tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select') {
      v = el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('name') || '';
      if (tag === 'input' && /^(submit|button|reset)$/i.test(el.type)) v = el.value || v;
      return clean(v, 120);
    }
    v = clean(el.innerText || el.textContent, 120);
    if (v) return v;
    var img = el.querySelector && el.querySelector('img[alt]');
    return clean(el.getAttribute('title') || el.getAttribute('alt') || (img ? img.getAttribute('alt') : ''), 120);
  }
  function ref(el) { var r = el.getAttribute('data-oa-ref'); if (!r) { r = 'e' + (++W.__oaSeq); el.setAttribute('data-oa-ref', r); } return r; }
  function find(r) { return D.querySelector('[data-oa-ref="' + String(r).replace(/[^A-Za-z0-9_-]/g, '') + '"]'); }
  function info(el) {
    var o = { ref: ref(el), role: roleOf(el), name: nameOf(el) };
    var tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select') {
      var ty = (el.getAttribute('type') || '').toLowerCase();
      if (ty === 'password') o.value = el.value ? '••••' : '';
      else if (ty === 'checkbox' || ty === 'radio') o.checked = !!el.checked;
      else o.value = clean(el.value, 200);
      if (ty) o.type = ty;
      if (tag === 'select') o.options = Array.prototype.slice.call(el.options, 0, 30).map(function (x) { return clean(x.text, 60); });
    }
    if (el.isContentEditable) o.value = clean(el.innerText, 200);
    if (el.getAttribute('aria-checked')) o.checked = el.getAttribute('aria-checked') === 'true';
    if (el.getAttribute('aria-expanded')) o.expanded = el.getAttribute('aria-expanded') === 'true';
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') o.disabled = true;
    if (tag === 'a' && el.href) o.href = clean(el.href, 200);
    return o;
  }
  if (op === 'read') {
    var seen = [], els = D.querySelectorAll(SEL), max = a.max || 250;
    for (var i = 0; i < els.length && seen.length < max; i++) {
      var el0 = els[i];
      if (!vis(el0)) continue;
      var ro = roleOf(el0);
      if (!ro) continue;
      var inf = info(el0);
      if (!inf.name && ro !== 'textbox' && ro !== 'combobox' && ro !== 'searchbox') continue;
      seen.push(inf);
    }
    var text = clean(D.body ? D.body.innerText : '', 0), mt = a.maxText || 8000;
    return { url: location.href, title: D.title, text: text.length > mt ? text.slice(0, mt) : text, truncated: text.length > mt, elements: seen, more: seen.length >= max };
  }
  var el = find(a.ref);
  if (!el) return { ok: false, error: 'no element ' + a.ref + ' on this page (call web.read again: the page changed)' };
  if (op === 'click') {
    var nm = nameOf(el);
    if (a.guard && new RegExp(a.guard, 'i').test(nm)) return { ok: false, sensitive: true, name: nm };
    el.scrollIntoView({ block: 'center' });
    if (el.focus) el.focus();
    el.click();
    return { ok: true, name: nm };
  }
  if (op === 'fill') {
    var tg = el.tagName.toLowerCase(), v = a.value == null ? '' : String(a.value);
    if (tg === 'input' && /^(file|password|hidden)$/i.test(el.type)) return { ok: false, error: 'cannot fill a ' + el.type + ' field' };
    el.scrollIntoView({ block: 'center' });
    if (el.focus) el.focus();
    if (tg === 'input' && /^(checkbox|radio)$/i.test(el.type)) {
      var want = /^(true|1|on|yes|checked)$/i.test(v);
      if (el.checked !== want) el.click();
      return { ok: true, checked: el.checked };
    }
    if (tg === 'select') {
      var opt = Array.prototype.find.call(el.options, function (o) { return o.value === v || clean(o.text) === clean(v); });
      if (!opt) return { ok: false, error: 'no option ' + v };
      el.value = opt.value;
    } else if (tg === 'input' || tg === 'textarea') {
      var proto = tg === 'input' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
      Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
    } else if (el.isContentEditable) {
      el.textContent = v;
    } else return { ok: false, error: 'element ' + a.ref + ' is not a field' };
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true };
  }
  if (op === 'highlight') {
    var old = D.querySelectorAll('[data-oa-hl]');
    for (var j = 0; j < old.length; j++) { old[j].style.outline = old[j].getAttribute('data-oa-hl'); old[j].style.outlineOffset = ''; old[j].removeAttribute('data-oa-hl'); }
    var tip = D.getElementById('__oa_tip'); if (tip) tip.remove();
    if (a.clear) return { ok: true };
    el.setAttribute('data-oa-hl', el.style.outline || '');
    el.style.outline = '3px solid #f59e0b'; el.style.outlineOffset = '3px';
    el.scrollIntoView({ block: 'center' });
    if (a.note) {
      tip = D.createElement('div'); tip.id = '__oa_tip'; tip.textContent = String(a.note).slice(0, 140);
      var r = el.getBoundingClientRect();
      tip.style.cssText = 'position:fixed;z-index:2147483647;background:#f59e0b;color:#111;font:600 13px -apple-system,system-ui,sans-serif;padding:6px 10px;border-radius:8px;box-shadow:0 4px 16px rgba(0,0,0,.25);pointer-events:none;max-width:320px;left:' + Math.max(8, Math.min(r.left, innerWidth - 330)) + 'px;top:' + (r.bottom + 50 > innerHeight ? Math.max(8, r.top - 40) : r.bottom + 10) + 'px';
      (D.body || D.documentElement).appendChild(tip);
    }
    return { ok: true, name: nameOf(el) };
  }
  return { ok: false, error: 'unknown op ' + op };
}"##;

/// The script evaluated in the page for one operation.
pub fn script(op: &str, args: &Value) -> String {
    format!(
        "(function(){{try{{return ({PAGE_LIB})({}, {});}}catch(e){{return {{ok:false,error:String(e&&e.message||e)}};}}}})()",
        serde_json::to_string(op).unwrap_or_default(),
        args
    )
}

fn sanitize_name(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let s: String = base.chars().map(|c| if c.is_alphanumeric() || ".-_ ()".contains(c) { c } else { '_' }).take(120).collect();
    let s = s.trim_start_matches('.').trim().to_string();
    if s.is_empty() { "download".into() } else { s }
}

fn unique(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (2..1000).map(|i| dir.join(format!("{stem}-{i}{ext}"))).find(|p| !p.exists()).unwrap_or(p)
}

#[derive(Default)]
pub struct Downloads {
    /// url → (placement, temp path, file name)
    pub pending: Mutex<std::collections::HashMap<String, (String, PathBuf, String)>>,
}

impl Kernel {
    /// A web placement id from `present:<id>`, `p_…`, `<module>/<view>`, or nothing (the top presented web view,
    /// else the only live one).
    pub fn resolve_web_placement(&self, want: Option<&str>) -> Result<String, HostError> {
        let live: Vec<String> = self.web_live.locked().iter().cloned().collect();
        let is_web = |pid: &str| self.web_view_of(pid).is_some();
        match want.map(str::trim).filter(|s| !s.is_empty()) {
            Some(w) if w.starts_with("present:") || w.starts_with("p_") => {
                if is_web(w) { Ok(w.to_string()) } else { Err(HostError::bad_request(format!("\"{w}\" is not a web view"))) }
            }
            Some(w) if w.starts_with("pr_") => {
                let pid = format!("present:{w}");
                if is_web(&pid) { Ok(pid) } else { Err(HostError::bad_request(format!("\"{w}\" is not a web view"))) }
            }
            Some(view) if view.contains('/') => {
                let g = self.lock();
                let pres = g.presented.iter().rev().find(|p| p["view"] == view).and_then(|p| p["id"].as_str().map(|id| format!("present:{id}")));
                let placed = || {
                    let (m, v) = view.split_once('/')?;
                    g.layout.placements.iter().find(|p| p.module == m && p.view == v).map(|p| p.id.clone())
                };
                pres.or_else(placed).ok_or_else(|| HostError::not_found(format!("view \"{view}\" is not open (view.open it as a sheet first)")))
            }
            Some(w) => Err(HostError::bad_request(format!("unknown placement \"{w}\""))),
            None => {
                let top = {
                    let g = self.lock();
                    g.presented.iter().rev().filter_map(|p| p["id"].as_str().map(|id| format!("present:{id}"))).find(|pid| live.contains(pid))
                };
                if let Some(t) = top.filter(|t| is_web(t)) {
                    return Ok(t);
                }
                match live.as_slice() {
                    [one] => Ok(one.clone()),
                    [] => Err(HostError::not_found("no web view is open (open one with view.open as a sheet)")),
                    _ => Err(HostError::bad_request(format!("several web views are open; pass \"placement\" ({})", live.join(", ")))),
                }
            }
        }
    }

    /// Evaluate a kernel script in a web view and parse its JSON result.
    pub async fn web_eval(&self, placement: &str, js: String) -> Result<Value, HostError> {
        let host = self.web_host.get().ok_or("web views are not available in this build (headless)")?.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        host.eval(placement, &js, Box::new(move |s| {
            let _ = tx.send(s);
        }))?;
        let s = tokio::time::timeout(Duration::from_secs(10), rx).await.map_err(|_| "the page did not answer in 10 s".to_string())?.map_err(|_| "the web view closed".to_string())?;
        if s.trim().is_empty() {
            return Err(HostError::upstream("the page script failed (the page may still be loading; try again)"));
        }
        Ok(serde_json::from_str(&s).map_err(|e| format!("bad page result: {e}"))?)
    }

    /// Granted per site (grant `web.control:<host>`; only an Allow is remembered).
    async fn web_control_grant(&self, host: &str, actor: &str) -> Result<(), HostError> {
        let who = crate::secrets::actor_words(self, actor);
        let req = crate::consent::ConsentReq::new(
            "web.control",
            format!("{who} wants to read and use {host} in the web view"),
            "It can read the page, fill fields and click links for you. Creating, enabling, accepting terms and granting access stay with you.",
        )
        .options(
            json!([
                { "optionId": "allow", "name": format!("Allow on {host}"), "kind": "allow_always" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
            ]),
            &["allow"],
        )
        .info(json!({ "host": host, "actor": actor }))
        .timeout(Duration::from_secs(300));
        if !crate::consent::granted(self, host, req, false, json!({ "actor": actor })).await.allowed {
            return Err(HostError::forbidden(format!("the user did not allow web control on {host}")));
        }
        Ok(())
    }

    /// `web.read {placement?}` · `web.click {placement?, ref}` · `web.fill {placement?, ref, value}` ·
    /// `web.highlight {placement?, ref, note?, clear?}`.
    pub async fn web_control(self: &Arc<Self>, op: &str, p: &Value, actor: &str) -> Result<Value, HostError> {
        let placement = self.resolve_web_placement(p.get("placement").and_then(Value::as_str))?;
        if !self.web_live.locked().contains(&placement) {
            return Err(HostError::conflict(format!("the web view {placement} is not open on screen yet")));
        }
        let url = self.lock().web.get(&placement).and_then(|w| w["url"].as_str().map(String::from)).unwrap_or_default();
        let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).ok_or_else(|| format!("the web view has no page yet ({url})"))?;
        self.web_control_grant(&host, actor).await?;
        let r#ref = p.get("ref").and_then(Value::as_str).map(String::from);
        let args = match op {
            "read" => json!({ "max": p.get("max").and_then(Value::as_u64).unwrap_or(250).min(500), "maxText": p.get("max_text").and_then(Value::as_u64).unwrap_or(8000).min(20000) }),
            "click" => json!({ "ref": r#ref.clone().ok_or(HostError::bad_request("web.click needs \"ref\" (from web.read)"))?, "guard": GUARD }),
            "fill" => json!({ "ref": r#ref.clone().ok_or(HostError::bad_request("web.fill needs \"ref\""))?, "value": p.get("value").cloned().unwrap_or(Value::Null) }),
            "highlight" => {
                let clear = p.get("clear").and_then(Value::as_bool).unwrap_or(false);
                json!({ "ref": r#ref.clone().or(if clear { Some(String::new()) } else { None }).ok_or(HostError::bad_request("web.highlight needs \"ref\""))?, "note": p.get("note"), "clear": clear })
            }
            o => return Err(HostError::bad_request(format!("unknown web op {o}"))),
        };
        let t0 = std::time::Instant::now();
        let mut r = self.web_eval(&placement, script(op, &args)).await;
        if op == "highlight" && args["clear"] == true && r.as_ref().is_ok_and(|v| v["ok"] == false) {
            r = Ok(json!({ "ok": true }));
        }
        self.research.log(actor, Kind::WebControl, json!({ "op": op, "host": host, "placement": placement, "ref": r#ref, "ok": r.as_ref().is_ok_and(|v| v["ok"] != false), "ms": t0.elapsed().as_millis() as u64 }));
        let mut v = r?;
        if v["sensitive"] == true {
            return Err(HostError::forbidden(format!(
                "\"{}\" is for the user to click (it creates, enables, accepts or grants something). Highlight it with web.highlight, say what it does in the guide step, and wait for their Done.",
                v["name"].as_str().unwrap_or("")
            )));
        }
        if v["ok"] == false {
            return Err(v["error"].as_str().unwrap_or("the page action failed").into());
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("placement".into(), json!(placement));
        }
        Ok(v)
    }

    // ---------- guide strip ----------

    /// `guide.set {placement?, steps: [{title, detail?}], current?}`.
    pub fn guide_set(&self, src: &[&Value]) -> Result<Value, String> {
        let get = |k: &str| src.iter().find_map(|v| v.get(k).cloned()).filter(|v| !v.is_null());
        let steps = get("steps").and_then(|v| v.as_array().cloned()).ok_or("guide.set needs \"steps\": [{title, detail?}]")?;
        if steps.is_empty() || steps.len() > 20 {
            return Err("guide.set needs 1 to 20 steps".into());
        }
        let mut clean = Vec::new();
        for s in steps {
            let title = s.get("title").and_then(Value::as_str).or_else(|| s.as_str()).map(str::trim).filter(|t| !t.is_empty()).ok_or("each step needs a \"title\"")?;
            let mut o = json!({ "title": title.chars().take(160).collect::<String>() });
            if let Some(d) = s.get("detail").and_then(Value::as_str) {
                o["detail"] = json!(d.chars().take(600).collect::<String>());
            }
            clean.push(o);
        }
        let current = get("current").and_then(|v| v.as_u64()).unwrap_or(0).min(clean.len() as u64 - 1);
        let placement = get("placement").and_then(|v| v.as_str().map(String::from)).map(|p| self.resolve_web_placement(Some(&p)).unwrap_or(p));
        let g = json!({ "placement": placement, "steps": clean, "current": current });
        self.lock().guide = Some(g.clone());
        self.research.log("agent", Kind::Guide, json!({ "op": "set", "steps": g["steps"].as_array().map(|a| a.len()), "current": current }));
        self.emit_snapshot();
        Ok(json!({ "ok": true, "guide": g }))
    }

    pub fn guide_clear(&self) {
        if self.lock().guide.take().is_some() {
            self.emit_snapshot();
        }
    }

    /// The user pressed Done on a step: tell the agent ("Done: <step title>") and move on.
    pub fn guide_done(self: &Arc<Self>, step: Option<u64>) -> Result<(), String> {
        let title = {
            let mut g = self.lock();
            let guide = g.guide.as_mut().ok_or("no guide is shown")?;
            let cur = guide["current"].as_u64().unwrap_or(0);
            let idx = step.unwrap_or(cur) as usize;
            let n = guide["steps"].as_array().map(|a| a.len()).unwrap_or(0);
            let title = guide["steps"].get(idx).and_then(|s| s["title"].as_str()).ok_or("no such step")?.to_string();
            if idx as u64 == cur && idx + 1 < n {
                guide["current"] = json!(cur + 1);
            }
            let mut done: Vec<Value> = guide["done"].as_array().cloned().unwrap_or_default();
            done.push(json!(idx));
            guide["done"] = json!(done);
            title
        };
        self.emit_snapshot();
        self.send_prompt(format!("Done: {title}"));
        Ok(())
    }

    // ---------- downloads ----------

    /// Download requested in a web view: where it goes first (kernel temp dir, outside the workspace).
    pub fn web_download_dest(&self, placement: &str, url: &str, suggested: &str) -> PathBuf {
        let dir = self.state_dir.join("downloads-tmp");
        let _ = std::fs::create_dir_all(&dir);
        let name = sanitize_name(if suggested.is_empty() { url.rsplit('/').next().unwrap_or("") } else { suggested });
        let path = dir.join(format!("{}-{name}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        self.prims.get::<Downloads>().pending.locked().insert(url.to_string(), (placement.to_string(), path.clone(), name));
        path
    }

    pub fn web_download_done(self: &Arc<Self>, url: &str, success: bool) {
        let Some((placement, path, name)) = self.prims.get::<Downloads>().pending.locked().remove(url) else { return };
        if !success {
            let _ = std::fs::remove_file(&path);
            self.notify(&format!("Download of {name} failed"));
            return;
        }
        let k = self.clone();
        self.spawn(async move {
            if let Err(e) = k.capture_download(Some(&placement), &path, &name).await {
                k.notify(&e);
            }
        });
    }

    /// Route a finished download: an OAuth client JSON becomes an `oauth-client` secret after a kernel card
    /// (the file never enters the workspace); anything else moves to the module's `data/downloads/`.
    pub async fn capture_download(self: &Arc<Self>, placement: Option<&str>, tmp: &Path, name: &str) -> Result<Value, String> {
        let bytes = std::fs::read(tmp).map_err(|e| format!("cannot read the download: {e}"))?;
        let module = placement.and_then(|p| self.placement_view(p)).map(|(m, _)| m);
        if crate::secrets::looks_like_oauth_client(&bytes) {
            let raw = String::from_utf8_lossy(&bytes).to_string();
            let res = crate::secrets::parse_oauth_client(&raw);
            let _ = std::fs::remove_file(tmp); // the client secret never lands in the workspace
            let (stored, meta) = res?;
            let provider = stored["provider"].as_str().unwrap_or("oauth").to_string();
            let what = crate::oauth::preset(&provider).map(|p| format!("{} OAuth client", p.name)).unwrap_or_else(|| "OAuth client".into());
            let project = meta.get("project_id").and_then(Value::as_str).map(|p| format!(" for project \"{p}\"")).unwrap_or_default();
            let mut secret_name = format!("{provider}-client");
            if let Some(m) = self.secrets.meta(&secret_name)
                && let Ok((_, old)) = self.secrets.get(&secret_name)
            {
                let same = serde_json::from_slice::<Value>(&old).ok().is_some_and(|o| o["client_id"] == stored["client_id"]);
                if !same && m["kind"] == "oauth-client" {
                    secret_name = (2..100).map(|i| format!("{provider}-client-{i}")).find(|n| self.secrets.meta(n).is_none()).unwrap_or(secret_name);
                }
            }
            let req = crate::consent::ConsentReq::new(
                "oauth_client_download",
                format!("Save {what}{project} to your Keychain?"),
                format!("You downloaded {name}. OpenApp keeps it in your Keychain as \"{secret_name}\"; the app and the agent never see the client secret."),
            )
            .options(
                json!([
                    { "optionId": "save", "name": "Save to Keychain", "kind": "allow_always" },
                    { "optionId": "discard", "name": "Discard", "kind": "reject_once" }
                ]),
                &["save"],
            )
            .info(json!({ "provider": provider, "file": name }))
            .timeout(Duration::from_secs(600));
            if !crate::consent::ask(self, req).await.allowed {
                return Err(format!("the OAuth client from {name} was not saved"));
            }
            let mut extra: Map<String, Value> = meta;
            extra.insert("label".into(), json!(format!("{what}{project}")));
            extra.insert("requested_by".into(), json!("download"));
            self.secrets.put(&secret_name, "oauth-client", stored.to_string().as_bytes(), extra)?;
            self.research.log("user", Kind::Secret, json!({ "phase": "saved", "name": secret_name, "kind": "oauth-client", "via": "download" }));
            self.system_message("system", &format!("Saved the {what} as \"{secret_name}\" in your Keychain."));
            self.emit_snapshot();
            return Ok(json!({ "ok": true, "secret": secret_name, "kind": "oauth-client" }));
        }
        let (dir, shown) = match &module {
            Some(m) => (self.workspace.join("modules").join(m).join("data").join("downloads"), format!("modules/{m}/data/downloads")),
            None => (self.workspace.join("downloads"), "downloads".to_string()),
        };
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = unique(&dir, &sanitize_name(name));
        if std::fs::rename(tmp, &dest).is_err() {
            std::fs::copy(tmp, &dest).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(tmp);
        }
        let rel = format!("{shown}/{}", dest.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
        self.research.log("user", Kind::Download, json!({ "module": module, "path": rel, "bytes": bytes.len() }));
        self.system_message("system", &format!("Downloaded {name} to {rel}."));
        Ok(json!({ "ok": true, "path": rel }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("client_secret_1.apps.json"), "client_secret_1.apps.json");
        assert_eq!(sanitize_name(".."), "download");
        assert!(script("read", &json!({})).starts_with("(function(){try{return (function (op, a)"));
    }
}
