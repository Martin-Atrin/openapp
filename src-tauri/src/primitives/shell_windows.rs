//! §21 G4 windows: a module view in its own native window (`window.open/close/focus/fullscreen`).
//! Each window loads the shell in window mode (`window.__OA_WINDOW__ = "<id>"`) and renders only that view with
//! placement id `window:<id>` (theme, channels, web views and actions work as in the main window).

use crate::research::Kind;
use crate::util::MutexExt;
use super::shell::{self, s, st};
use super::*;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

pub fn primitive() -> Primitive {
    let open = handler(|k, p, c| async move { open(&k, &p, &c).await });
    let close = handler(|k, p, c| async move { with_window(&k, &p, &c, "close") });
    let focus = handler(|k, p, c| async move { with_window(&k, &p, &c, "focus") });
    let full = handler(|k, p, c| async move { with_window(&k, &p, &c, "fullscreen") });
    Primitive {
        name: "window",
        purpose: "Open a module view in its own native window (resizable, full screen, always on top)",
        example: r#"{ "host": "window.open", "view": "daw/editor", "title": "Mini DAW", "fullscreen": true } → { "windowId": "w_…" }"#,
        class: Class::Free,
        actions: vec![
            Action { name: "window.open", args: "{ view: \"<module>/<view>\", title?, width?, height?, fullscreen?, alwaysOnTop?, id? } → { windowId }", handler: open.clone() },
            Action { name: "window.close", args: "{ id? } (default: the window the control is in)", handler: close.clone() },
            Action { name: "window.focus", args: "{ id? }", handler: focus.clone() },
            Action { name: "window.fullscreen", args: "{ id?, on? } (toggles without on)", handler: full.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "window.open", args: "{ view, title?, width?, height?, fullscreen?, alwaysOnTop?, id? }", handler: open },
            Endpoint { method: "window.close", args: "{ id }", handler: close },
            Endpoint { method: "window.focus", args: "{ id }", handler: focus },
            Endpoint { method: "window.fullscreen", args: "{ id, on? }", handler: full },
        ],
        data: vec![DataSource {
            name: "windows",
            doc: "module windows: [{ id, view, title, fullscreen, alwaysOnTop, pending? }] (pending = opens when OpenApp's window does)",
            get: data_fn(|k| Value::Array(st(k).windows.locked().values().cloned().collect())),
        }],
        ..Default::default()
    }
}

pub fn label(id: &str) -> String {
    format!("win-{id}")
}

/// `window:<id>` → (module, view) for placement lookups.
pub fn placement_view(k: &Kernel, id: &str) -> Option<(String, String)> {
    let w = st(k).windows.locked().get(id).cloned()?;
    let (m, v) = w["view"].as_str()?.split_once('/')?;
    Some((m.to_string(), v.to_string()))
}

fn valid_id(id: &str) -> bool {
    crate::util::valid_ident(id, 48)
}

async fn open(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let view = s(p, "view").ok_or("window.open needs \"view\": \"<module>/<view>\"")?.to_string();
    let (module, vname) = view.split_once('/').ok_or("\"view\" must be \"<module>/<view>\"")?;
    if let Some(m) = caller.module()
        && m != module
    {
        return Err(format!("\"{m}\" may open only its own views in windows, not \"{view}\""));
    }
    {
        let g = k.lock();
        let m = g.modules.get(module).ok_or_else(|| format!("unknown module \"{module}\""))?;
        if !m.views.contains_key(vname) {
            return Err(format!("module \"{module}\" has no view \"{vname}\" (views: {})", m.views.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
    }
    let id = match s(p, "id") {
        Some(i) if valid_id(i) => i.to_string(),
        Some(i) => return Err(format!("bad window id \"{i}\" (letters, digits, - and _)")),
        None => shell::next_id(k, "w_"),
    };
    let title = s(p, "title").map(String::from).unwrap_or_else(|| k.lock().modules.get(module).map(|m| m.name.clone()).unwrap_or_else(|| module.to_string()));
    let width = p.get("width").and_then(Value::as_f64).unwrap_or(960.0).clamp(240.0, 8000.0);
    let height = p.get("height").and_then(Value::as_f64).unwrap_or(640.0).clamp(160.0, 8000.0);
    let fullscreen = p.get("fullscreen").and_then(Value::as_bool).unwrap_or(false);
    let on_top = p.get("alwaysOnTop").and_then(Value::as_bool).unwrap_or(false);
    let visible = p.get("visible").and_then(Value::as_bool).unwrap_or(true);
    let entry = json!({ "id": id, "view": view, "module": module, "title": title, "width": width, "height": height, "fullscreen": fullscreen, "alwaysOnTop": on_top, "visible": visible });
    if !shell::attached(k) {
        // no app window in this run: the window opens when OpenApp's window does (`open_pending`)
        let mut e = entry;
        e["pending"] = json!(true);
        st(k).windows.locked().insert(id.clone(), e);
        k.research.log(&caller.id, Kind::Window, json!({ "op": "open", "view": view, "fullscreen": fullscreen, "pending": true }));
        k.emit_snapshot();
        return Ok(json!({ "ok": true, "windowId": id, "applied": false, "pending": true, "note": shell::PENDING_NOTE }));
    }
    let app = shell::app(k)?;
    if let Some(w) = app.get_webview_window(&label(&id)) {
        // same id: update and focus
        let _ = w.set_title(&title);
        let _ = w.set_fullscreen(fullscreen);
        let _ = w.set_always_on_top(on_top);
        let _ = w.set_focus();
        st(k).windows.locked().insert(id.clone(), entry);
        k.emit_snapshot();
        return Ok(json!({ "ok": true, "windowId": id, "reused": true, "applied": true }));
    }
    create(k, &app, &id, entry)?;
    k.research.log(&caller.id, Kind::Window, json!({ "op": "open", "view": view, "fullscreen": fullscreen }));
    Ok(json!({ "ok": true, "windowId": id, "applied": true }))
}

/// Build the native window of a stored entry (the shell renders the view as soon as the snapshot lists it).
fn create(k: &Arc<Kernel>, app: &tauri::AppHandle, id: &str, mut entry: Value) -> Result<(), String> {
    if let Some(o) = entry.as_object_mut() {
        o.remove("pending");
    }
    let f = |key: &str, d: f64| entry.get(key).and_then(Value::as_f64).unwrap_or(d);
    let b = |key: &str, d: bool| entry.get(key).and_then(Value::as_bool).unwrap_or(d);
    let (width, height, fullscreen, on_top, visible) = (f("width", 960.0), f("height", 640.0), b("fullscreen", false), b("alwaysOnTop", false), b("visible", true));
    let title = entry["title"].as_str().unwrap_or("OpenApp").to_string();
    st(k).windows.locked().insert(id.to_string(), entry);
    k.emit_snapshot();
    let init = format!("window.__OA_WINDOW__ = {};", json!(id));
    let builder = WebviewWindowBuilder::new(app, label(id), WebviewUrl::App("index.html".into()))
        .title(&title)
        .inner_size(width, height)
        .min_inner_size(240.0, 160.0)
        .fullscreen(fullscreen)
        .always_on_top(on_top)
        .visible(visible)
        .initialization_script(&init);
    let w = match builder.build() {
        Ok(w) => w,
        Err(e) => {
            st(k).windows.locked().remove(id);
            k.emit_snapshot();
            return Err(format!("could not open the window: {e}"));
        }
    };
    let weak = Arc::downgrade(k);
    let wid = id.to_string();
    w.on_window_event(move |ev| {
        if let tauri::WindowEvent::Destroyed = ev
            && let Some(k) = weak.upgrade()
        {
            closed(&k, &wid);
        }
    });
    Ok(())
}

/// Attach: open the windows requested while there was no app window.
pub fn open_pending(k: &Arc<Kernel>) {
    let Ok(app) = shell::app(k) else { return };
    let pending: Vec<(String, Value)> = st(k).windows.locked().iter().filter(|(_, v)| v["pending"] == true).map(|(a, b)| (a.clone(), b.clone())).collect();
    for (id, entry) in pending {
        if let Err(e) = create(k, &app, &id, entry) {
            log::warn!("pending window {id}: {e}");
        }
    }
}

/// The window was closed (by the user or window.close): forget it, close its web surfaces.
fn closed(k: &Kernel, id: &str) {
    let removed = st(k).windows.locked().remove(id);
    if let Some(w) = removed {
        k.research.log("user", Kind::Window, json!({ "op": "closed", "view": w["view"] }));
        if let Some(host) = k.web_host.get() {
            host.destroy(&format!("window:{id}"));
        }
        k.emit_snapshot();
    }
}

fn with_window(k: &Arc<Kernel>, p: &Value, caller: &Caller, op: &str) -> Result<Value, String> {
    let id = s(p, "id")
        .map(String::from)
        .or_else(|| caller.placement.as_deref().and_then(|pl| pl.strip_prefix("window:")).map(String::from))
        .ok_or_else(|| format!("window.{op} needs \"id\" (from window.open)"))?;
    let entry = st(k).windows.locked().get(&id).cloned().ok_or_else(|| format!("no window \"{id}\""))?;
    if let Some(m) = caller.module()
        && entry["module"] != m
    {
        return Err(format!("window \"{id}\" belongs to another module"));
    }
    if entry["pending"] == true {
        // not open yet (no app window): change the stored request
        let mut out = json!({ "ok": true, "windowId": id, "applied": false, "pending": true, "note": shell::PENDING_NOTE });
        match op {
            "close" => {
                st(k).windows.locked().remove(&id);
                out["pending"] = json!(false);
                out["closed"] = json!(true);
            }
            "fullscreen" => {
                let on = p.get("on").and_then(Value::as_bool).unwrap_or_else(|| entry["fullscreen"] != true);
                if let Some(e) = st(k).windows.locked().get_mut(&id) {
                    e["fullscreen"] = json!(on);
                }
                out["fullscreen"] = json!(on);
            }
            _ => {}
        }
        k.emit_snapshot();
        return Ok(out);
    }
    let app = shell::app(k)?;
    let w = app.get_webview_window(&label(&id)).ok_or_else(|| format!("window \"{id}\" is not open"))?;
    match op {
        "close" => {
            w.close().map_err(|e| e.to_string())?;
            closed(k, &id);
        }
        "focus" => {
            let _ = w.unminimize();
            let _ = w.show();
            w.set_focus().map_err(|e| e.to_string())?;
        }
        _ => {
            let on = p.get("on").and_then(Value::as_bool).unwrap_or_else(|| !w.is_fullscreen().unwrap_or(false));
            w.set_fullscreen(on).map_err(|e| e.to_string())?;
            if let Some(e) = st(k).windows.locked().get_mut(&id) {
                e["fullscreen"] = json!(on);
            }
            k.emit_snapshot();
            return Ok(json!({ "ok": true, "windowId": id, "fullscreen": on }));
        }
    }
    Ok(json!({ "ok": true, "windowId": id }))
}

/// Webview window label → placement prefix for web surfaces (`main` or `win-<id>`).
pub fn window_label_for_placement(placement: &str) -> String {
    match placement.strip_prefix("window:") {
        Some(id) => label(id),
        None => "main".into(),
    }
}
