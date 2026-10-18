//! G6 background (contract §21).
//! - OpenApp keeps running with the main window closed when any job, `restart: "always"` service, host-module
//!   service (`"host": {"service": true}`) or background grant exists ([`keep_running`]; the main window's close
//!   handler in shell_misc.rs asks it). Then a kernel menu-bar item shows it ("OpenApp keeps running: …", Show,
//!   Quit) unless a module's tray item (shell_tray.rs) already does.
//! - `background.request { module?, reason? }` → Granted card per module ("keep running in the background").
//! - `app.login_item { enabled? }` → SMAppService.mainApp (in-process, ServiceManagement); enabling from the agent
//!   or a module shows a Granted card. Without `enabled` it returns the status. Works for the bundled app; a dev
//!   binary outside an .app answers `not_found`.

use crate::research::Kind;
use crate::kernel::HostError;
use crate::util::MutexExt;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const BG_TRAY: &str = "openapp-bg";

#[derive(Default)]
struct Bg {
    tray_on: Mutex<Option<String>>,
    started: AtomicBool,
    login: Mutex<Option<String>>,
}

fn bg(k: &Kernel) -> Arc<Bg> {
    k.prims.get::<Bg>()
}

/// Why OpenApp keeps running in the background (empty = it quits when the window closes).
pub fn reasons(k: &Kernel) -> Vec<String> {
    let mut out = Vec::new();
    let jobs = super::jobs::count(k);
    if jobs > 0 {
        out.push(format!("{jobs} job{}", if jobs == 1 { "" } else { "s" }));
    }
    let (mut svcs, mut hosts) = (0, 0);
    for m in k.lock().modules.values().filter(|m| m.status == "running" || m.status == "idle") {
        let Some(mf) = &m.manifest else { continue };
        svcs += mf.raw.get("services").and_then(Value::as_array).map(|a| a.iter().filter(|s| s["restart"] == "always").count()).unwrap_or(0);
        if super::host_service::is_service(mf) {
            hosts += 1;
        }
    }
    if svcs > 0 {
        out.push(format!("{svcs} service{}", if svcs == 1 { "" } else { "s" }));
    }
    if hosts > 0 {
        out.push(format!("{hosts} Mac service{}", if hosts == 1 { "" } else { "s" }));
    }
    let granted: Vec<String> = k.grants.of_kind("background").into_iter().filter(|(_, v)| v["allowed"] == true).map(|(m, _)| m).collect();
    let known: Vec<String> = granted.into_iter().filter(|m| k.lock().modules.contains_key(m)).collect();
    if !known.is_empty() {
        out.push(format!("background: {}", known.join(", ")));
    }
    out
}

pub fn keep_running(k: &Kernel) -> bool {
    !reasons(k).is_empty()
}

/// Create, update or remove the kernel's background menu-bar item.
pub fn refresh(k: &Kernel) {
    let Ok(app) = super::shell::app(k) else { return };
    let r = reasons(k);
    let want = (!r.is_empty() && !super::shell_tray::has_tray(k)).then(|| format!("OpenApp keeps running: {}", r.join(", ")));
    let b = bg(k);
    let mut cur = b.tray_on.locked();
    if *cur == want {
        return;
    }
    match &want {
        None => {
            let a = app.clone();
            let _ = app.run_on_main_thread(move || drop(a.remove_tray_by_id(BG_TRAY)));
        }
        Some(label) => {
            use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
            let menu = (|| -> tauri::Result<Menu<tauri::Wry>> {
                let m = Menu::new(&app)?;
                m.append(&MenuItem::with_id(&app, "oa:bg:info", label, false, None::<&str>)?)?;
                m.append(&PredefinedMenuItem::separator(&app)?)?;
                m.append(&MenuItem::with_id(&app, "oa:t:show", "Show OpenApp", true, None::<&str>)?)?;
                m.append(&MenuItem::with_id(&app, "oa:t:quit", "Quit OpenApp", true, None::<&str>)?)?;
                Ok(m)
            })();
            let Ok(menu) = menu else { return };
            if let Some(t) = app.tray_by_id(BG_TRAY) {
                let _ = t.set_menu(Some(menu));
                let _ = t.set_tooltip(Some(label.as_str()));
            } else {
                let mut tb = tauri::tray::TrayIconBuilder::with_id(BG_TRAY).menu(&menu).tooltip(label).show_menu_on_left_click(true);
                if let Some(i) = app.default_window_icon() {
                    tb = tb.icon(i.clone().to_owned());
                }
                if let Err(e) = tb.build(&app) {
                    log::warn!("background tray: {e}");
                    return;
                }
            }
        }
    }
    *cur = want;
}

async fn run_loop(w: std::sync::Weak<Kernel>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let Some(k) = w.upgrade() else { return };
        if k.is_shutdown() {
            return;
        }
        refresh(&k);
    }
}

// ---------- login item (SMAppService) ----------

#[cfg(target_os = "macos")]
mod sm {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSError;

    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {}

    fn service() -> Result<*mut AnyObject, String> {
        let cls = AnyClass::get(c"SMAppService").ok_or("SMAppService is not available on this macOS")?;
        let svc: *mut AnyObject = unsafe { msg_send![cls, mainAppService] };
        if svc.is_null() { Err("SMAppService.mainApp is not available".into()) } else { Ok(svc) }
    }

    pub fn status() -> Result<isize, String> {
        let svc = service()?;
        Ok(unsafe { msg_send![svc, status] })
    }

    pub fn set(enabled: bool) -> Result<isize, String> {
        let svc = service()?;
        let mut err: *mut NSError = std::ptr::null_mut();
        let ok: Bool = unsafe {
            if enabled { msg_send![svc, registerAndReturnError: &mut err as *mut *mut NSError] } else { msg_send![svc, unregisterAndReturnError: &mut err as *mut *mut NSError] }
        };
        if !ok.as_bool() {
            let why = if err.is_null() { "unknown error".to_string() } else { unsafe { (*err).localizedDescription() }.to_string() };
            return Err(why);
        }
        status()
    }
}

#[cfg(not(target_os = "macos"))]
mod sm {
    pub fn status() -> Result<isize, String> {
        Err("login items need macOS".into())
    }
    pub fn set(_: bool) -> Result<isize, String> {
        Err("login items need macOS".into())
    }
}

fn status_word(s: isize) -> &'static str {
    match s {
        0 => "not_registered",
        1 => "enabled",
        2 => "requires_approval",
        _ => "not_found",
    }
}

fn login_status(k: &Kernel) -> Value {
    let s = sm::status();
    let w = s.as_ref().map(|s| status_word(*s).to_string()).unwrap_or_else(|e| format!("unavailable: {e}"));
    *bg(k).login.locked() = Some(w.clone());
    json!(w)
}

async fn login_item(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, HostError> {
    let Some(enabled) = p.get("enabled").and_then(Value::as_bool) else {
        return Ok(json!({ "ok": true, "status": login_status(&k) }));
    };
    if enabled && c.id != "user" {
        super::shell::consent(
            &k,
            "login_item",
            "openapp",
            &format!("{} wants OpenApp to open when you log in", crate::secrets::actor_words(&k, c.module().map(|m| format!("module:{m}")).as_deref().unwrap_or(&c.id))),
            "OpenApp then starts with your Mac, so jobs and services keep running. You can change this in System Settings → General → Login Items.",
        )
        .await?;
    }
    let r = tokio::task::spawn_blocking(move || sm::set(enabled)).await.map_err(|e| e.to_string())?;
    k.research.log(&c.id, Kind::LoginItem, json!({ "enabled": enabled, "ok": r.is_ok() }));
    match r {
        Ok(s) => {
            let w = status_word(s);
            *bg(&k).login.locked() = Some(w.into());
            k.emit_snapshot();
            let note = (w == "requires_approval").then_some("the user approves it in System Settings → General → Login Items");
            Ok(json!({ "ok": true, "status": w, "note": note }))
        }
        Err(e) => Err(HostError::upstream(format!("could not change the login item: {e} (a development build outside OpenApp.app cannot register itself)"))),
    }
}

async fn background_request(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, HostError> {
    let module = super::shell::owner(&k, &c, &p)?;
    let name = k.lock().modules.get(&module).map(|m| m.name.clone()).unwrap_or_else(|| module.clone());
    let reason = arg(&p, "reason").unwrap_or("so it can keep working while the window is closed");
    super::shell::consent(&k, "background", &module, &format!("Keep {name} running in the background?"), &format!("{name} asks to keep running {reason}. Closing the window then keeps OpenApp in the menu bar; Quit there stops it.")).await?;
    refresh(&k);
    k.emit_snapshot();
    Ok(json!({ "ok": true, "module": module, "keep_running": keep_running(&k) }))
}

fn on_event(k: &Kernel, ev: &Event) {
    match ev {
        Event::Started => {
            if !bg(k).started.swap(true, Ordering::SeqCst)
                && let Some(ka) = k.arc()
            {
                k.spawn(run_loop(Arc::downgrade(&ka)));
            }
        }
        Event::ModuleLoaded(_) | Event::ModuleUnloaded(_) => refresh(k),
        _ => {}
    }
}

pub fn primitive() -> Primitive {
    let login = handler(login_item);
    let req = handler(background_request);
    Primitive {
        name: "background",
        purpose: "Keep OpenApp running with the window closed while jobs, services or background grants exist (menu-bar item shows it), and open it at login (SMAppService).",
        example: r#"{ "host": "background.request", "reason": "to sync mail every 15 minutes" } · { "host": "app.login_item", "enabled": true } · spec: { "type": "text", "value": { "$host": "background", "path": "summary" } }"#,
        class: Class::Granted,
        actions: vec![
            Action { name: "background.request", args: "{ module?, reason? } — Granted card: keep this module running in the background", handler: req.clone() },
            Action { name: "app.login_item", args: "{ enabled? } → { status: enabled | not_registered | requires_approval | not_found } (enabling: Granted card)", handler: login.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "background.request", args: "{ reason? }", handler: req },
            Endpoint { method: "app.login_item", args: "{ enabled? }", handler: login },
        ],
        data: vec![DataSource {
            name: "background",
            doc: "{ keep_running, reasons[], summary, login_item } — whether closing the window keeps OpenApp running",
            get: data_fn(|k| {
                let r = reasons(k);
                let login = bg(k).login.locked().clone();
                json!({ "keep_running": !r.is_empty(), "summary": if r.is_empty() { "Quits when the window closes".to_string() } else { format!("Keeps running: {}", r.join(", ")) }, "reasons": r, "login_item": login })
            }),
        }],
        on_event: Some(Arc::new(on_event)),
        describe_keys: vec![("background", data_fn(|k| json!({ "keep_running": keep_running(k), "reasons": reasons(k), "login_item": login_status(k) })))],
        ..Default::default()
    }
}
