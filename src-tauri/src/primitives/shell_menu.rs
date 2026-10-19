//! §21 G4 menus: `menu.set` builds the app menu bar from the kernel's fixed OpenApp menu (About, Provider…,
//! Accounts…, Safe mode, Quit), the standard Edit / View / Window menus, then every module's menus (merged by title).
//! A click runs the item's action as the owning module (placement id `menu`).

use crate::research::Kind;
use crate::util::MutexExt;
use super::shell::{self, st};
use super::shell_shortcuts::Accel;
use super::*;
use tauri::menu::{AboutMetadata, CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

pub fn primitive() -> Primitive {
    let set = handler(|k, p, c| async move { set(&k, &p, &c) });
    Primitive {
        name: "menu",
        purpose: "Add menus to the macOS menu bar (merged under OpenApp's own menus); items run module actions",
        example: r#"{ "host": "menu.set", "menus": [{ "title": "Track", "items": [{ "label": "Play", "shortcut": "Space", "action": { "tool": "play" } }, "separator", { "label": "Loop", "checked": true, "action": { "tool": "toggle_loop" } }] }] }"#,
        class: Class::Free,
        actions: vec![Action { name: "menu.set", args: "{ menus: [{ title, items: [{ label, shortcut?, action, enabled?, checked?, items? } | \"separator\"] }] } ([] clears)", handler: set.clone() }],
        endpoints: vec![Endpoint { method: "menu.set", args: "{ menus }", handler: set }],
        data: vec![DataSource {
            name: "menus",
            doc: "menu bar menus set by modules: { <module>: menus }",
            get: data_fn(|k| json!(*st(k).menus.locked())),
        }],
        ..Default::default()
    }
}

/// Accelerators the kernel menus own (reported as conflicts).
pub const KERNEL_ACCELS: &[&str] = &["Cmd+Q", "Cmd+H", "Cmd+Alt+H", "Cmd+Z", "Cmd+Shift+Z", "Cmd+X", "Cmd+C", "Cmd+V", "Cmd+A", "Cmd+M", "Cmd+W", "Cmd+Ctrl+F"];

/// Validate a menus array; returns the list of problems (empty = ok) and the accelerators used.
pub fn validate(menus: &Value) -> (Vec<String>, Vec<(String, String)>) {
    let mut errs = Vec::new();
    let mut accels = Vec::new();
    let Some(arr) = menus.as_array() else {
        return (vec!["\"menus\" must be an array".into()], accels);
    };
    fn items(v: &Value, path: &str, errs: &mut Vec<String>, accels: &mut Vec<(String, String)>, depth: usize) {
        let Some(list) = v.as_array() else {
            errs.push(format!("{path}: \"items\" must be an array"));
            return;
        };
        for (i, it) in list.iter().enumerate() {
            let here = format!("{path}[{i}]");
            if it == "separator" || it.get("type").and_then(Value::as_str) == Some("separator") {
                continue;
            }
            let Some(label) = it.get("label").and_then(Value::as_str) else {
                errs.push(format!("{here}: needs \"label\" (or the string \"separator\")"));
                continue;
            };
            if let Some(sub) = it.get("items") {
                if depth >= 3 {
                    errs.push(format!("{here}: submenus nest at most 3 levels"));
                } else {
                    items(sub, &format!("{here}.items"), errs, accels, depth + 1);
                }
                continue;
            }
            if !it.get("action").is_some_and(Value::is_object) {
                errs.push(format!("{here} \"{label}\": needs \"action\" ({{tool, args}} | {{host, …}} | {{channel, data}})"));
            }
            if let Some(sc) = it.get("shortcut").and_then(Value::as_str) {
                match Accel::parse(sc) {
                    Ok(a) => accels.push((a.canonical(), label.to_string())),
                    Err(e) => errs.push(format!("{here} \"{label}\": {e}")),
                }
            }
        }
    }
    for (i, m) in arr.iter().enumerate() {
        let Some(t) = m.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
            errs.push(format!("menus[{i}]: needs \"title\""));
            continue;
        };
        items(m.get("items").unwrap_or(&Value::Null), &format!("menus[{i}] \"{t}\""), &mut errs, &mut accels, 1);
    }
    (errs, accels)
}

fn set(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p)?;
    let menus = p.get("menus").cloned().unwrap_or_else(|| json!([]));
    let (errs, accels) = validate(&menus);
    if !errs.is_empty() {
        return Err(errs.join("; "));
    }
    let mut conflicts = Vec::new();
    {
        let s = st(k);
        let others = s.menus.locked();
        let mut theirs = Vec::new();
        for (m, v) in others.iter().filter(|(m, _)| **m != module) {
            theirs.extend(validate(v).1.into_iter().map(|(a, l)| (a, format!("{m}: {l}"))));
        }
        for (a, label) in &accels {
            if KERNEL_ACCELS.contains(&a.as_str()) {
                conflicts.push(json!({ "accelerator": a, "item": label, "with": "OpenApp menu" }));
            }
            if let Some((_, o)) = theirs.iter().find(|(x, _)| x == a) {
                conflicts.push(json!({ "accelerator": a, "item": label, "with": o }));
            }
        }
    }
    {
        let s = st(k);
        let mut m = s.menus.locked();
        if menus.as_array().is_some_and(Vec::is_empty) {
            m.remove(&module);
        } else {
            m.insert(module.clone(), menus);
        }
    }
    shell::save(k);
    k.research.log(&caller.id, Kind::Menu, json!({ "module": module, "conflicts": conflicts.len() }));
    let applied = apply(k);
    k.emit_snapshot();
    let mut out = json!({ "ok": true, "module": module, "conflicts": conflicts });
    shell::applied_out(k, &mut out, applied);
    Ok(out)
}

/// Build and install the whole menu bar (kernel menus + module menus).
pub fn apply(k: &Kernel) -> Result<(), String> {
    let app = shell::app(k)?;
    let menu = build(k, &app).map_err(|e| format!("menu: {e}"))?;
    app.set_menu(menu).map_err(|e| format!("menu: {e}"))?;
    Ok(())
}

type R = tauri::Wry;

fn build(k: &Kernel, app: &tauri::AppHandle) -> tauri::Result<Menu<R>> {
    let sep = || PredefinedMenuItem::separator(app);
    let safe_on = super::lifecycle::safe_mode(k);
    let about = PredefinedMenuItem::about(app, Some("About OpenApp"), Some(AboutMetadata { name: Some("OpenApp".into()), version: Some(env!("CARGO_PKG_VERSION").into()), ..Default::default() }))?;
    let provider = MenuItem::with_id(app, "oa:k:provider", "Provider…", true, None::<&str>)?;
    let accounts = MenuItem::with_id(app, "oa:k:accounts", "Accounts…", true, None::<&str>)?;
    let safe = CheckMenuItem::with_id(app, "oa:k:safe", "Safe Mode", true, safe_on, None::<&str>)?;
    let app_menu = Submenu::with_items(
        app,
        "OpenApp",
        true,
        &[&about, &sep()?, &provider, &accounts, &safe, &sep()?, &PredefinedMenuItem::services(app, None)?, &sep()?, &PredefinedMenuItem::hide(app, None)?, &PredefinedMenuItem::hide_others(app, None)?, &PredefinedMenuItem::show_all(app, None)?, &sep()?, &PredefinedMenuItem::quit(app, None)?],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[&PredefinedMenuItem::undo(app, None)?, &PredefinedMenuItem::redo(app, None)?, &sep()?, &PredefinedMenuItem::cut(app, None)?, &PredefinedMenuItem::copy(app, None)?, &PredefinedMenuItem::paste(app, None)?, &PredefinedMenuItem::select_all(app, None)?],
    )?;
    let view = Submenu::with_items(app, "View", true, &[&PredefinedMenuItem::fullscreen(app, None)?])?;
    let window = Submenu::with_items(app, "Window", true, &[&PredefinedMenuItem::minimize(app, None)?, &PredefinedMenuItem::maximize(app, None)?, &sep()?, &PredefinedMenuItem::close_window(app, None)?])?;
    let menu = Menu::with_items(app, &[&app_menu, &edit, &view])?;
    // module menus, merged by title (modules in id order)
    let s = st(k);
    let mods = s.menus.locked().clone();
    let mut actions = s.item_actions.locked();
    actions.retain(|id, _| !id.starts_with("oa:m:"));
    let mut by_title: Vec<(String, Submenu<R>, bool)> = Vec::new();
    let mut n = 0usize;
    for (module, menus) in &mods {
        for m in menus.as_array().into_iter().flatten() {
            let title = m["title"].as_str().unwrap_or("Menu").to_string();
            let idx = match by_title.iter().position(|(t, _, _)| *t == title) {
                Some(i) => {
                    by_title[i].1.append(&sep()?)?;
                    i
                }
                None => {
                    by_title.push((title.clone(), Submenu::new(app, &title, true)?, false));
                    by_title.len() - 1
                }
            };
            let sub = by_title[idx].1.clone();
            add_items(app, &sub, &m["items"], module, &mut actions, &mut n, 0)?;
            by_title[idx].2 = true;
        }
    }
    for (_, sub, _) in &by_title {
        menu.append(sub)?;
    }
    menu.append(&window)?;
    Ok(menu)
}

fn add_items(app: &tauri::AppHandle, sub: &Submenu<R>, items: &Value, module: &str, actions: &mut std::collections::HashMap<String, (String, Value)>, n: &mut usize, depth: usize) -> tauri::Result<()> {
    for it in items.as_array().into_iter().flatten() {
        if it == "separator" || it.get("type").and_then(Value::as_str) == Some("separator") {
            sub.append(&PredefinedMenuItem::separator(app)?)?;
            continue;
        }
        let label = it["label"].as_str().unwrap_or("Item");
        let enabled = it.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        if let Some(children) = it.get("items").filter(|_| depth < 3) {
            let child = Submenu::new(app, label, enabled)?;
            add_items(app, &child, children, module, actions, n, depth + 1)?;
            sub.append(&child)?;
            continue;
        }
        *n += 1;
        let id = format!("oa:m:{n}");
        let accel = it.get("shortcut").and_then(Value::as_str).and_then(|a| Accel::parse(a).ok()).map(|a| a.canonical());
        let item: Box<dyn IsMenuItem<R>> = match it.get("checked").and_then(Value::as_bool) {
            Some(c) => Box::new(CheckMenuItem::with_id(app, &id, label, enabled, c, accel.as_deref())?),
            None => Box::new(MenuItem::with_id(app, &id, label, enabled, accel.as_deref())?),
        };
        sub.append(item.as_ref())?;
        actions.insert(id, (module.to_string(), it.get("action").cloned().unwrap_or(Value::Null)));
    }
    Ok(())
}

/// App-level menu event (menu bar and tray menus).
pub fn on_menu_event(k: &Arc<Kernel>, id: &str) {
    match id {
        "oa:k:provider" => {
            let _ = super::shell_misc::show_main(k);
            shell::emit_shell(k, json!({ "op": "provider" }));
        }
        "oa:k:accounts" => {
            let _ = super::shell_misc::show_main(k);
            shell::emit_shell(k, json!({ "op": "accounts" }));
        }
        "oa:k:safe" => {
            // §21 G9 toggle through the lifecycle primitive's host action
            let on = !super::lifecycle::safe_mode(k);
            let k = k.clone();
            k.clone().spawn(async move {
                match super::call_action(&k, "safe.set", json!({ "on": on }), Caller::new("user")).await {
                    Some(Ok(_)) => {}
                    Some(Err(e)) => k.notify(&e),
                    None => k.notify("Safe mode is not available in this build."),
                }
                let _ = apply(&k);
            });
        }
        "oa:t:show" => {
            let _ = super::shell_misc::show_main(k);
        }
        "oa:t:quit" => {
            if let Ok(app) = shell::app(k) {
                app.exit(0);
            }
        }
        _ => {
            let found = st(k).item_actions.locked().get(id).cloned();
            if let Some((module, action)) = found {
                let placement = if id.starts_with("oa:t:") { "tray" } else { "menu" };
                let k = k.clone();
                k.clone().spawn(async move {
                    let _ = shell::run_action(&k, placement, &module, &action, json!({})).await;
                });
            }
        }
    }
}
