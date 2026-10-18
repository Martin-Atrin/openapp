//! §21 G4 native shell — shared state and helpers for the `shell_*` primitives (windows, menus, tray,
//! shortcuts, notifications, misc) and `files` (G5).
//!
//! The Tauri app handle is attached by the Tauri layer (`attach`). The kernel owns the desired state (tray, menus,
//! shortcuts, dock badge, windows to open): a call stores it and applies it when a window exists; without one
//! (headless runs, before the window opens) it answers `{ applied: false, pending: true }` and `attach` applies
//! everything later. Describe key `native_shell` and host data show the desired state. Registrations (menus,
//! tray, shortcuts) are kept per owning module in `<state>/shell.json` and restored on start; they are dropped
//! when the module unloads.

use crate::research::Kind;
use crate::util::MutexExt;
use super::{Caller, Kernel};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
pub struct ShellState {
    pub app: OnceLock<tauri::AppHandle>,
    /// G4 windows: id → `{id, view, module, title, width, height, fullscreen, alwaysOnTop}`.
    pub windows: Mutex<BTreeMap<String, Value>>,
    /// menu.set per owning module: module → `menus` array.
    pub menus: Mutex<BTreeMap<String, Value>>,
    /// Menu / tray item id → (module, action).
    pub item_actions: Mutex<HashMap<String, (String, Value)>>,
    /// tray.set (one tray item; the last module that set it owns it).
    pub tray: Mutex<Option<Value>>,
    /// shortcut id → `{id, module, accelerator, key, global, action, when}`.
    pub shortcuts: Mutex<BTreeMap<String, Value>>,
    /// notification id → `{module, action, actions}` (until answered).
    pub notes: Mutex<HashMap<String, Value>>,
    /// dock.badge text (desired; applied on attach).
    pub badge: Mutex<Option<String>>,
    /// Lucide icons rendered by the shell (PNG): name → bytes; names still wanted.
    pub icons: Mutex<HashMap<String, Vec<u8>>>,
    pub icons_wanted: Mutex<BTreeSet<String>>,
    pub seq: AtomicU64,
}

pub fn st(k: &Kernel) -> Arc<ShellState> {
    k.prims.get::<ShellState>()
}

/// The Tauri app, or a clear error in headless runs.
pub fn app(k: &Kernel) -> Result<tauri::AppHandle, String> {
    st(k).app.get().cloned().ok_or_else(|| "the native shell is not available in this run (no app window)".to_string())
}

/// A native shell (app window) is attached.
pub fn attached(k: &Kernel) -> bool {
    st(k).app.get().is_some()
}

pub const PENDING_NOTE: &str = "stored; no app window in this run: OpenApp applies it when its window opens";

/// Mark a stored-state result: `applied: true`, or (no window) `applied: false, pending: true`, or the apply error.
pub fn applied_out(k: &Kernel, out: &mut Value, applied: Result<(), String>) {
    match applied {
        Ok(()) => out["applied"] = json!(true),
        Err(_) if !attached(k) => {
            out["applied"] = json!(false);
            out["pending"] = json!(true);
            out["note"] = json!(PENDING_NOTE);
        }
        Err(e) => {
            out["applied"] = json!(false);
            out["note"] = json!(e);
        }
    }
}

/// Describe key `native_shell`: the desired native state the kernel holds, and whether a window applies it.
pub fn describe(k: &Kernel) -> Value {
    let s = st(k);
    let windows: Vec<Value> = s.windows.locked().values().cloned().collect();
    let shortcuts: Vec<Value> = s
        .shortcuts
        .locked()
        .values()
        .map(|v| json!({ "id": v["id"], "module": v["module"], "accelerator": v["accelerator"], "global": v["global"] }))
        .collect();
    json!({
        "attached": attached(k),
        "note": if attached(k) { "applied to the app" } else { "no app window in this run: this state is stored and applied when OpenApp's window opens" },
        "tray": *s.tray.locked(),
        "menus": *s.menus.locked(),
        "shortcuts": shortcuts,
        "dock_badge": *s.badge.locked(),
        "windows": windows,
    })
}

pub fn next_id(k: &Kernel, prefix: &str) -> String {
    let n = st(k).seq.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{}{n}", &uuid::Uuid::new_v4().simple().to_string()[..6])
}

/// The module that owns a registration: the caller's module, or `params.module` for agent / user / kernel calls.
pub fn owner(k: &Kernel, caller: &Caller, p: &Value) -> Result<String, String> {
    if let Some(m) = caller.module() {
        return Ok(m.to_string());
    }
    let m = p.get("module").and_then(Value::as_str).ok_or("pass \"module\": \"<module id>\" (the module that owns this)")?;
    if !k.lock().modules.contains_key(m) {
        return Err(format!("unknown module \"{m}\""));
    }
    Ok(m.to_string())
}

pub fn s<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str).filter(|v| !v.is_empty())
}

/// `openapp_snapshot` lines for the native shell (empty when nothing is set).
pub fn summary_text(k: &Kernel) -> String {
    let s = st(k);
    let mut lines = Vec::new();
    if let Some(t) = s.tray.locked().as_ref() {
        let items = t["menu"].as_array().map(|m| m.iter().filter_map(|i| i["label"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        lines.push(format!("tray ({}): icon {} title {:?} menu [{items}]", t["module"].as_str().unwrap_or("?"), t["icon"], t["title"].as_str().unwrap_or("")));
    }
    for (m, menus) in s.menus.locked().iter() {
        let titles = menus.as_array().map(|a| a.iter().filter_map(|x| x["title"].as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        lines.push(format!("menus ({m}): {titles}"));
    }
    for v in s.shortcuts.locked().values() {
        lines.push(format!("shortcut ({}): {}{}", v["module"].as_str().unwrap_or("?"), v["accelerator"].as_str().unwrap_or("?"), if v["global"] == true { " global" } else { "" }));
    }
    if let Some(b) = s.badge.locked().as_ref() {
        lines.push(format!("dock badge: {b}"));
    }
    for w in s.windows.locked().values() {
        lines.push(format!("window {} {}{}", w["id"].as_str().unwrap_or("?"), w["view"].as_str().unwrap_or("?"), if w["pending"] == true { " (opens with the app window)" } else { " (open)" }));
    }
    if lines.is_empty() {
        return String::new();
    }
    let head = if attached(k) { "\nNative shell (applied):\n".to_string() } else { "\nNative shell (stored; no app window in this run — applied when it opens):\n".to_string() };
    head + &lines.iter().map(|l| format!("  - {l}\n")).collect::<String>()
}

// ---------- persistence ----------

fn shell_path(k: &Kernel) -> PathBuf {
    k.state_dir.join("shell.json")
}

pub fn save(k: &Kernel) {
    let s = st(k);
    let v = json!({
        "menus": *s.menus.locked(),
        "tray": *s.tray.locked(),
        "shortcuts": *s.shortcuts.locked(),
    });
    let _ = std::fs::write(shell_path(k), serde_json::to_vec_pretty(&v).unwrap_or_default());
}

/// Load `<state>/shell.json` into memory (once per kernel).
pub fn load(k: &Kernel) {
    let v: Value = std::fs::read(shell_path(k)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let s = st(k);
    if let Some(m) = v.get("menus").and_then(Value::as_object) {
        *s.menus.locked() = m.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
    }
    if let Some(t) = v.get("tray").filter(|t| t.is_object()) {
        *s.tray.locked() = Some(t.clone());
    }
    if let Some(m) = v.get("shortcuts").and_then(Value::as_object) {
        *s.shortcuts.locked() = m.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
    }
}

/// Drop everything a module registered (module unloaded).
pub fn forget_module(k: &Kernel, module: &str) {
    let s = st(k);
    let had_menu = s.menus.locked().remove(module).is_some();
    let had_tray = {
        let mut t = s.tray.locked();
        if t.as_ref().is_some_and(|t| t["module"] == module) {
            *t = None;
            true
        } else {
            false
        }
    };
    let gone: Vec<Value> = {
        let mut sc = s.shortcuts.locked();
        let ids: Vec<String> = sc.iter().filter(|(_, v)| v["module"] == module).map(|(id, _)| id.clone()).collect();
        ids.iter().filter_map(|id| sc.remove(id)).collect()
    };
    if !(had_menu || had_tray || !gone.is_empty()) {
        return;
    }
    if had_menu {
        let _ = super::shell_menu::apply(k);
    }
    if had_tray {
        let _ = super::shell_tray::apply(k);
    }
    for g in gone.iter().filter(|g| g["global"] == true) {
        super::shell_shortcuts::unregister_global(k, g);
    }
    save(k);
}

// ---------- consent (Granted class) ----------

/// The stored decision of `kind` for a module (grant store).
pub fn grant(k: &Kernel, kind: &str, module: &str) -> Option<bool> {
    k.grants.allowed(kind, module)
}

/// Granted-class consent per module: remembered answer, else a kernel card (Allow / Deny), stored.
pub async fn consent(k: &Arc<Kernel>, kind: &str, module: &str, title: &str, text: &str) -> Result<(), crate::kernel::HostError> {
    crate::consent::module_grant(k, kind, module, title, text).await
}

// ---------- running a module action ----------

/// Run an action owned by `module` (menu item, tray click, shortcut, notification answer, deep link, file open).
/// `{tool, args}` → module tool (input merged into args); `{host, …}` → host action; `{channel, data?}` → publish.
pub async fn run_action(k: &Arc<Kernel>, placement: &str, module: &str, action: &Value, input: Value) -> Result<Value, String> {
    k.research.log("user", Kind::ShellAction, json!({ "placement": placement, "module": module, "kind": action.get("tool").map(|_| "tool").or(action.get("host").map(|_| "host")).or(action.get("channel").map(|_| "channel")) }));
    let res = run_inner(k, placement, module, action, input).await;
    if let Err(e) = &res {
        k.push_ui_error(placement, Some(module), &format!("action failed: {e}"));
        k.notify(&format!("{module}: {e}"));
    }
    res
}

async fn run_inner(k: &Arc<Kernel>, placement: &str, module: &str, action: &Value, input: Value) -> Result<Value, String> {
    if let Some(tool) = s(action, "tool") {
        let mut args = action.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
        if let Value::Object(i) = input {
            args.extend(i);
        }
        let r = k.call_module_tool(module, tool, args).await?;
        k.after_module_call(module, &r, true);
        if r.is_error == Some(true) {
            return Err(format!("tool {module}/{tool} returned an error: {}", crate::modules::result_text(&r)));
        }
        return Ok(serde_json::to_value(&r).unwrap_or(Value::Null));
    }
    if let Some(topic) = s(action, "channel") {
        let mut data = action.get("data").cloned().unwrap_or_else(|| json!({}));
        if let (Some(d), Value::Object(i)) = (data.as_object_mut(), input) {
            d.extend(i);
        }
        publish(k, module, topic, data)?;
        return Ok(json!({ "ok": true, "published": topic }));
    }
    if let Some(host) = s(action, "host") {
        let mut params = action.as_object().cloned().unwrap_or_default();
        params.remove("host");
        if let Value::Object(i) = &input {
            params.extend(i.clone());
        }
        if super::action(host).is_some() && !super::host_actions().contains(&host) {
            let caller = Caller { id: format!("view:{module}"), placement: Some(placement.to_string()) };
            return super::call_action(k, host, Value::Object(params), caller).await.unwrap_or_else(|| Err(format!("unknown host action \"{host}\"")));
        }
        let mut a = Value::Object(params);
        a["host"] = json!(host);
        return k.ui_action_value(placement, a, None).await;
    }
    Err("action needs \"tool\", \"host\" or \"channel\"".into())
}

/// Publish on a live channel (G1). Modules publish only on their own topics; `kernel/*` is the kernel's.
pub fn publish(k: &Arc<Kernel>, owner: &str, topic: &str, data: Value) -> Result<(), String> {
    if !(topic.starts_with(&format!("{owner}/")) || (owner == "kernel" && topic.starts_with("kernel/"))) {
        return Err(format!("\"{owner}\" may publish only on \"{owner}/…\" topics, not \"{topic}\""));
    }
    super::channels::publish(k, topic, super::channels::ChannelData::Json(data));
    Ok(())
}

/// Kernel → shell event `oa://shell` (window-mode helpers: provider picker, focus).
pub fn emit_shell(k: &Kernel, payload: Value) {
    k.sink.emit("oa://shell", payload);
}
