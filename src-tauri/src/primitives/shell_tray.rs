//! §21 G4 tray: one menu-bar (status) item, e.g. a mini player. `tray.set { icon, title?, tooltip?, menu?, action? }`,
//! `tray.clear`. `icon` is a lucide name (the shell renders it to a template PNG) or an image path in the workspace.
//! While a tray item exists, closing the main window hides it instead of quitting (the tray menu has Show / Quit).

use crate::research::Kind;
use crate::util::MutexExt;
use super::shell::{self, s, st};
use super::*;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

const TRAY_ID: &str = "openapp-tray";
type R = tauri::Wry;

pub fn primitive() -> Primitive {
    let set = handler(|k, p, c| async move { set(&k, &p, &c) });
    let clear = handler(|k, p, c| async move { clear(&k, &p, &c) });
    Primitive {
        name: "tray",
        purpose: "A menu-bar item (icon + optional title) with a menu or a click action, e.g. a mini player",
        example: r#"{ "host": "tray.set", "icon": "music", "title": "▶ Song", "menu": [{ "label": "Play/Pause", "action": { "tool": "toggle" } }, "separator", { "label": "Open player", "action": { "host": "window.open", "view": "player/mini", "width": 320, "height": 120, "alwaysOnTop": true } }] }"#,
        class: Class::Free,
        actions: vec![
            Action { name: "tray.set", args: "{ icon: \"<lucide name>\" | \"<image path>\", title?, tooltip?, menu?: [{ label, action, enabled?, checked? } | \"separator\"], action? (left click) }", handler: set.clone() },
            Action { name: "tray.clear", args: "{}", handler: clear.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "tray.set", args: "{ icon, title?, tooltip?, menu?, action? }", handler: set },
            Endpoint { method: "tray.clear", args: "{}", handler: clear },
        ],
        data: vec![
            DataSource {
                name: "tray",
                doc: "the menu-bar item: { module, icon, title, menu } or null",
                get: data_fn(|k| st(k).tray.locked().clone().unwrap_or(Value::Null)),
            },
            DataSource {
                name: "shell.icons",
                doc: "(shell internal) lucide icons the kernel wants rendered to PNG",
                get: data_fn(|k| json!(*st(k).icons_wanted.locked())),
            },
        ],
        commands: vec![("shell.icon", handler(|k, p, _c| async move { icon_rendered(&k, &p) }))],
        ..Default::default()
    }
}

fn set(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p)?;
    let icon = s(p, "icon").ok_or("tray.set needs \"icon\" (lucide name like \"music\", or an image path)")?.to_string();
    if let Some(menu) = p.get("menu") {
        let (errs, _) = super::shell_menu::validate(&json!([{ "title": "tray", "items": menu }]));
        if !errs.is_empty() {
            return Err(errs.join("; ").replace("menus[0] \"tray\"", "menu"));
        }
    }
    if let Some(a) = p.get("action")
        && !a.is_object()
    {
        return Err("\"action\" must be an action object".into());
    }
    let image_path = if looks_like_path(&icon) { Some(crate::paths::map_path(&k.workspace, &icon, true)?) } else { None };
    let mut entry = json!({ "module": module, "icon": icon, "title": p.get("title"), "tooltip": p.get("tooltip"), "menu": p.get("menu").cloned().unwrap_or(json!([])), "action": p.get("action") });
    if let Some(ip) = &image_path {
        entry["iconPath"] = json!(ip.to_string_lossy());
    } else if !st(k).icons.locked().contains_key(&icon) {
        st(k).icons_wanted.locked().insert(icon.clone());
    }
    let prev = st(k).tray.locked().replace(entry);
    shell::save(k);
    k.research.log(&caller.id, Kind::Tray, json!({ "op": "set", "module": module, "replaced": prev.as_ref().map(|p| p["module"].clone()) }));
    let applied = apply(k);
    k.emit_snapshot();
    let mut out = json!({ "ok": true, "module": module });
    if let Some(p) = prev.filter(|p| p["module"] != module.as_str()) {
        out["replaced"] = p["module"].clone();
    }
    shell::applied_out(k, &mut out, applied);
    Ok(out)
}

fn clear(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p).ok();
    {
        let s = st(k);
        let mut t = s.tray.locked();
        match (&*t, &module) {
            (None, _) => return Ok(json!({ "ok": true, "cleared": false })),
            (Some(cur), Some(m)) if cur["module"] != m.as_str() => return Err(format!("the tray item belongs to \"{}\"", cur["module"].as_str().unwrap_or("?"))),
            _ => *t = None,
        }
    }
    shell::save(k);
    let mut out = json!({ "ok": true, "cleared": true });
    shell::applied_out(k, &mut out, apply(k));
    k.emit_snapshot();
    Ok(out)
}

fn looks_like_path(icon: &str) -> bool {
    icon.contains('/') || [".png", ".jpg", ".jpeg", ".ico"].iter().any(|e| icon.to_ascii_lowercase().ends_with(e))
}

/// Shell command `shell.icon { name, png }` (base64 PNG of a lucide icon, rendered black on transparent).
fn icon_rendered(k: &Arc<Kernel>, p: &Value) -> Result<Value, String> {
    let name = s(p, "name").ok_or("shell.icon needs \"name\"")?.to_string();
    let png = crate::kernel::base64_decode(s(p, "png").ok_or("shell.icon needs \"png\"")?).ok_or("bad base64")?;
    Image::from_bytes(&png).map_err(|e| format!("not a PNG: {e}"))?;
    st(k).icons.locked().insert(name.clone(), png);
    st(k).icons_wanted.locked().remove(&name);
    let uses = st(k).tray.locked().as_ref().is_some_and(|t| t["icon"] == name.as_str());
    if uses {
        let _ = apply(k);
    }
    k.emit_snapshot();
    Ok(json!({ "ok": true }))
}

pub fn has_tray(k: &Kernel) -> bool {
    st(k).tray.locked().is_some()
}

/// Create, update or remove the native tray item from the stored entry.
pub fn apply(k: &Kernel) -> Result<(), String> {
    let app = shell::app(k)?;
    let entry = st(k).tray.locked().clone();
    let Some(t) = entry else {
        // the status item must be released on the main thread (AppKit)
        let a = app.clone();
        app.run_on_main_thread(move || drop(a.remove_tray_by_id(TRAY_ID))).map_err(|e| e.to_string())?;
        return Ok(());
    };
    let module = t["module"].as_str().unwrap_or("").to_string();
    // icon: image path, rendered lucide PNG (template), else the app icon until the shell renders it
    let (img, template): (Option<Image<'static>>, bool) = if let Some(p) = t["iconPath"].as_str() {
        (Image::from_path(p).ok().map(|i| i.to_owned()), false)
    } else {
        let png = st(k).icons.locked().get(t["icon"].as_str().unwrap_or("")).cloned();
        match png {
            Some(b) => (Image::from_bytes(&b).ok().map(|i| i.to_owned()), true),
            None => (app.default_window_icon().map(|i| i.clone().to_owned()), false),
        }
    };
    let menu = build_menu(k, &app, &module, &t["menu"]).map_err(|e| format!("tray menu: {e}"))?;
    let title = t["title"].as_str().map(String::from);
    let tooltip = t["tooltip"].as_str().map(String::from);
    let click = t.get("action").filter(|a| a.is_object()).is_some();
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Some(i) = img {
            tray.set_icon(Some(i)).map_err(|e| e.to_string())?;
        }
        let _ = tray.set_icon_as_template(template);
        let _ = tray.set_title(title.as_deref());
        let _ = tray.set_tooltip(tooltip.as_deref());
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
        let _ = tray.set_show_menu_on_left_click(!click);
        return Ok(());
    }
    let weak = k.arc().map(|a| Arc::downgrade(&a));
    let mut b = TrayIconBuilder::with_id(TRAY_ID).menu(&menu).show_menu_on_left_click(!click).icon_as_template(template);
    if let Some(i) = img {
        b = b.icon(i);
    }
    if let Some(t) = &title {
        b = b.title(t);
    }
    if let Some(t) = &tooltip {
        b = b.tooltip(t);
    }
    b.on_tray_icon_event(move |_tray, ev| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
            let Some(k) = weak.as_ref().and_then(|w| w.upgrade()) else { return };
            let entry = st(&k).tray.locked().clone();
            if let Some(t) = entry
                && let Some(a) = t.get("action").filter(|a| a.is_object()).cloned()
            {
                let m = t["module"].as_str().unwrap_or("").to_string();
                let k2 = k.clone();
                k.spawn(async move {
                    let _ = shell::run_action(&k2, "tray", &m, &a, json!({})).await;
                });
            }
        }
    })
    .build(&app)
    .map_err(|e| format!("could not create the tray item: {e}"))?;
    Ok(())
}

fn build_menu(k: &Kernel, app: &tauri::AppHandle, module: &str, items: &Value) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(app)?;
    let s = st(k);
    let mut actions = s.item_actions.locked();
    actions.retain(|id, _| !id.starts_with("oa:t:") || id == "oa:t:show" || id == "oa:t:quit");
    let mut n = 0;
    for it in items.as_array().into_iter().flatten() {
        if it == "separator" || it.get("type").and_then(Value::as_str) == Some("separator") {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            continue;
        }
        n += 1;
        let id = format!("oa:t:{n}");
        let label = it["label"].as_str().unwrap_or("Item");
        let enabled = it.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let item: Box<dyn IsMenuItem<R>> = match it.get("checked").and_then(Value::as_bool) {
            Some(c) => Box::new(CheckMenuItem::with_id(app, &id, label, enabled, c, None::<&str>)?),
            None => Box::new(MenuItem::with_id(app, &id, label, enabled, None::<&str>)?),
        };
        menu.append(item.as_ref())?;
        actions.insert(id, (module.to_string(), it.get("action").cloned().unwrap_or(Value::Null)));
    }
    if n > 0 {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    menu.append(&MenuItem::with_id(app, "oa:t:show", "Show OpenApp", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "oa:t:quit", "Quit OpenApp", true, None::<&str>)?)?;
    Ok(menu)
}
