//! §21 G4 everyday OS glue: dock badge, clipboard, print, share sheet, drag and drop,
//! deep links `openapp://<module>/<path>` and "Open with OpenApp" file handlers. Also the Tauri attach point
//! for all shell primitives (`attach`, `on_run_event`).

use crate::research::Kind;
use crate::kernel::HostError;
use crate::util::MutexExt;
use super::shell::{self, s, st};
use super::*;
use std::path::{Path, PathBuf};
use tauri::Manager;

pub fn primitive() -> Primitive {
    let badge = handler(|k, p, _c| async move { dock_badge(&k, &p) });
    let cw = handler(|k, p, c| async move { clipboard_write(&k, &p, &c) });
    let cr = handler(|k, p, c| async move { clipboard_read(&k, &p, &c).await });
    let print = handler(|k, p, c| async move { print(&k, &p, &c).await });
    let share = handler(|k, p, c| async move { share(&k, &p, &c) });
    Primitive {
        name: "os",
        purpose: "Everyday macOS glue: dock badge, clipboard, print, share sheet, drag and drop, deep links openapp://<module>/…, file handlers",
        example: r#"{ "host": "clipboard.write", "text": "copied!" } · { "host": "share", "path": "/workspace/modules/notes/data/out.pdf" } · manifest "deeplinks": ["open"], "opens": [".md", "text/*"]"#,
        class: Class::Free,
        actions: vec![
            Action { name: "dock.badge", args: "{ text | null }", handler: badge.clone() },
            Action { name: "clipboard.write", args: "{ text } | { image: <path> | b64 PNG }", handler: cw.clone() },
            Action { name: "clipboard.read", args: "{} → { text?, image? (path in module data) } (Granted: one card per module)", handler: cr.clone() },
            Action { name: "print", args: "{ view: \"<module>/<view>\" | url | path (PDF/image) }", handler: print.clone() },
            Action { name: "share", args: "{ path | url | text } (macOS share sheet)", handler: share.clone() },
            Action { name: "files.drag_out", args: "{ path } (start dragging a workspace or granted-folder file out of OpenApp; call on mousedown)", handler: handler(|k, p, c| async move { drag_out(&k, &p, &c) }) },
        ],
        endpoints: vec![
            Endpoint { method: "dock.badge", args: "{ text | null }", handler: badge },
            Endpoint { method: "clipboard.write", args: "{ text } | { image }", handler: cw },
            Endpoint { method: "clipboard.read", args: "{}", handler: cr },
            Endpoint { method: "print", args: "{ view | url | path }", handler: print },
            Endpoint { method: "share", args: "{ path | url | text }", handler: share },
        ],
        commands: vec![("files.drop", handler(|k, p, c| async move { files_drop(&k, &p, &c) }))],
        describe_keys: vec![("native_shell", data_fn(shell::describe))],
        data: vec![DataSource {
            name: "dock",
            doc: "the dock badge: { badge } (text or null)",
            get: data_fn(|k| json!({ "badge": *st(k).badge.locked() })),
        }],
        // a deleted module loses its menus, tray item and shortcuts (an idle or reloaded one keeps them)
        on_event: Some(Arc::new(|k: &Kernel, ev: &Event| {
            if let Event::Started = ev {
                // desired native state survives restarts (also in headless runs: describe shows it)
                shell::load(k);
            }
            if let Event::ModuleUnloaded(id) = ev
                && !k.workspace.join("modules").join(id).join("openapp.json").exists()
            {
                shell::forget_module(k, id);
            }
        })),
        ..Default::default()
    }
}

// ---------- Tauri attach ----------

/// Called once by the Tauri layer after the kernel exists (main window created from tauri.conf.json).
pub fn attach(k: &Arc<Kernel>, app: &tauri::AppHandle) {
    let _ = st(k).app.set(app.clone());
    for r in [
        app.plugin(tauri_plugin_dialog::init()),
        app.plugin(tauri_plugin_clipboard_manager::init()),
        app.plugin(tauri_plugin_global_shortcut::Builder::new().build()),
    ] {
        if let Err(e) = r {
            log::warn!("shell plugin: {e}");
        }
    }
    shell::load(k);
    let weak = Arc::downgrade(k);
    app.on_menu_event(move |_app, ev| {
        if let Some(k) = weak.upgrade() {
            super::shell_menu::on_menu_event(&k, ev.id().as_ref());
        }
    });
    if let Err(e) = super::shell_menu::apply(k) {
        log::warn!("{e}");
    }
    if let Err(e) = super::shell_tray::apply(k) {
        log::warn!("{e}");
    }
    super::shell_shortcuts::restore(k);
    // desired state stored before the window existed
    if st(k).badge.locked().is_some()
        && let Err(e) = apply_badge(k)
    {
        log::warn!("dock badge: {e}");
    }
    super::shell_windows::open_pending(k);
    if let Some(w) = app.get_webview_window("main") {
        let weak = Arc::downgrade(k);
        let w2 = w.clone();
        w.on_window_event(move |ev| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = ev
                && weak.upgrade().is_some_and(|k| super::shell_tray::has_tray(&k) || super::background::keep_running(&k))
            {
                // a tray item keeps OpenApp running (Show OpenApp / Quit in its menu)
                api.prevent_close();
                let _ = w2.hide();
            }
        });
    }
}

/// From `App::run`: deep links / opened files (macOS `application:openURLs:`), dock click.
pub fn on_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent) {
    let Some(k) = app.try_state::<Arc<Kernel>>().map(|s| s.inner().clone()) else { return };
    match event {
        tauri::RunEvent::Opened { urls } => {
            let urls: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
            let k2 = k.clone();
            k.spawn(async move {
                for u in urls {
                    if let Err(e) = opened(&k2, &u).await {
                        k2.notify(&e);
                    }
                }
            });
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { has_visible_windows: false, .. } => {
            let _ = show_main(&k);
        }
        _ => {}
    }
}

pub fn show_main(k: &Kernel) -> Result<(), String> {
    let app = shell::app(k)?;
    let w = app.get_webview_window("main").ok_or("main window not found")?;
    let _ = w.unminimize();
    w.show().map_err(|e| e.to_string())?;
    w.set_focus().map_err(|e| e.to_string())
}

// ---------- dock badge, clipboard ----------

fn dock_badge(k: &Arc<Kernel>, p: &Value) -> Result<Value, String> {
    let text = match p.get("text") {
        Some(Value::String(t)) if !t.is_empty() => Some(t.chars().take(12).collect::<String>()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    *st(k).badge.locked() = text.clone();
    let mut out = json!({ "ok": true, "badge": text });
    shell::applied_out(k, &mut out, apply_badge(k));
    k.emit_snapshot();
    Ok(out)
}

/// Show the stored dock badge on the main window.
fn apply_badge(k: &Kernel) -> Result<(), String> {
    let app = shell::app(k)?;
    let w = app.get_webview_window("main").ok_or("main window not found")?;
    w.set_badge_label(st(k).badge.locked().clone()).map_err(|e| e.to_string())
}

fn clipboard_write(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let app = shell::app(k)?;
    if let Some(t) = p.get("text").and_then(Value::as_str) {
        app.clipboard().write_text(t.to_string()).map_err(|e| e.to_string())?;
        k.research.log(&caller.id, Kind::Clipboard, json!({ "op": "write", "kind": "text", "bytes": t.len() }));
        return Ok(json!({ "ok": true }));
    }
    let img = s(p, "image").ok_or("clipboard.write needs \"text\" or \"image\" (path or base64 PNG)")?;
    let bytes = if img.len() > 200 && !img.contains('/') {
        crate::kernel::base64_decode(img).ok_or("bad base64 image")?
    } else {
        let host = crate::paths::map_any(k, img, true)?;
        std::fs::read(&host).map_err(|e| e.to_string())?
    };
    let image = tauri::image::Image::from_bytes(&bytes).map_err(|e| format!("not a PNG image: {e}"))?;
    app.clipboard().write_image(&image).map_err(|e| e.to_string())?;
    k.research.log(&caller.id, Kind::Clipboard, json!({ "op": "write", "kind": "image", "bytes": bytes.len() }));
    Ok(json!({ "ok": true }))
}

async fn clipboard_read(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, HostError> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let module = shell::owner(k, caller, p)?;
    let name = k.lock().modules.get(&module).map(|m| m.name.clone()).unwrap_or_else(|| module.clone());
    shell::consent(k, "clipboard.read", &module, &format!("{name} wants to read the clipboard"), "The module can then read what you copy whenever it asks. You can revoke this in grants.").await?;
    let app = shell::app(k)?;
    let mut out = json!({ "ok": true });
    if let Ok(t) = app.clipboard().read_text() {
        out["text"] = json!(t);
    }
    if out.get("text").is_none()
        && let Ok(img) = app.clipboard().read_image()
    {
        let (w, h) = (img.width(), img.height());
        let png = encode_png(img.rgba(), w, h)?;
        let rel = format!("modules/{module}/data/clipboard/clip-{}.png", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
        let host = k.workspace.join(&rel);
        std::fs::create_dir_all(host.parent().unwrap_or(&k.workspace)).map_err(|e| e.to_string())?;
        std::fs::write(&host, png).map_err(|e| e.to_string())?;
        out["image"] = json!(format!("/workspace/{rel}"));
    }
    k.research.log(&caller.id, Kind::Clipboard, json!({ "op": "read", "module": module, "text": out.get("text").is_some(), "image": out.get("image").is_some() }));
    Ok(out)
}

/// Minimal PNG encoder (RGBA8, stored deflate blocks) so no image crate is needed.
pub fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err("bad image buffer".into());
    }
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for b in data {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc(&c).to_be_bytes());
    }
    let mut raw = Vec::with_capacity(rgba.len() + h as usize);
    for row in rgba.chunks(w as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib: stored blocks of ≤ 65535 bytes + adler32
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(if i + 1 == blocks.len() { 1 } else { 0 });
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut bsum) = (1u32, 0u32);
    for x in &raw {
        a = (a + *x as u32) % 65521;
        bsum = (bsum + a) % 65521;
    }
    z.extend_from_slice(&((bsum << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

// ---------- print ----------

async fn print(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let app = shell::app(k)?;
    if let Some(view) = s(p, "view") {
        // a window already showing the view, else a new window with it; then the native print panel
        let existing = st(k).windows.locked().values().find(|w| w["view"] == view).and_then(|w| w["id"].as_str().map(String::from));
        let id = match existing {
            Some(id) => id,
            None => {
                let r = super::call_action(k, "window.open", json!({ "view": view, "title": p.get("title").cloned().unwrap_or(json!("Print")), "width": 820, "height": 1000 }), caller.clone()).await.unwrap_or_else(|| Err("window.open missing".into()))?;
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await; // let the view render
                r["windowId"].as_str().unwrap_or("").to_string()
            }
        };
        let w = app.get_webview_window(&super::shell_windows::label(&id)).ok_or("print window is gone")?;
        w.print().map_err(|e| e.to_string())?;
        return Ok(json!({ "ok": true, "windowId": id }));
    }
    if let Some(url) = s(p, "url") {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("print { url } needs an http(s) URL".into());
        }
        let label = format!("print-{}", shell::next_id(k, ""));
        let parsed: tauri::Url = url.parse().map_err(|e| format!("bad url: {e}"))?;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::External(parsed))
            .title("Print")
            .inner_size(820.0, 1000.0)
            .on_page_load(move |w, pl| {
                if matches!(pl.event(), tauri::webview::PageLoadEvent::Finished) && !done.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let _ = w.print();
                }
            })
            .build()
            .map_err(|e| e.to_string())?;
        let _ = w;
        return Ok(json!({ "ok": true }));
    }
    let path = s(p, "path").ok_or("print needs \"view\", \"url\" or \"path\"")?;
    let host = crate::paths::map_any(k, path, true)?;
    let ext = host.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if !["pdf", "png", "jpg", "jpeg", "gif", "tiff", "heic"].contains(&ext.as_str()) {
        return Err("print { path } prints PDFs and images (Preview); show other files in a view and use print { view }".into());
    }
    let script = format!("tell application \"Preview\"\nactivate\nprint POSIX file {}\nend tell", json!(host.to_string_lossy()));
    let out = tokio::process::Command::new("osascript").args(["-e", &script]).output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("print failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(json!({ "ok": true, "via": "Preview" }))
}

// ---------- share sheet ----------

fn share(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let app = shell::app(k)?;
    let item = if let Some(path) = s(p, "path") {
        ShareItem::File(crate::paths::map_any(k, path, true)?)
    } else if let Some(url) = s(p, "url") {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("share { url } needs an http(s) URL".into());
        }
        ShareItem::Url(url.to_string())
    } else {
        ShareItem::Text(s(p, "text").ok_or("share needs \"path\", \"url\" or \"text\"")?.to_string())
    };
    let label = caller.placement.as_deref().map(super::shell_windows::window_label_for_placement).unwrap_or_else(|| "main".into());
    let w = app.get_webview_window(&label).or_else(|| app.get_webview_window("main")).ok_or("no window to show the share sheet in")?;
    let view = w.ns_view().map_err(|e| e.to_string())? as usize;
    k.research.log(&caller.id, Kind::Share, json!({ "kind": match &item { ShareItem::File(_) => "file", ShareItem::Url(_) => "url", ShareItem::Text(_) => "text" } }));
    w.run_on_main_thread(move || show_share_picker(view as *mut std::ffi::c_void, item)).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

pub enum ShareItem {
    File(PathBuf),
    Url(String),
    Text(String),
}

#[cfg(target_os = "macos")]
fn show_share_picker(view: *mut std::ffi::c_void, item: ShareItem) {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSSharingServicePicker, NSView};
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSRectEdge, NSSize, NSString, NSURL};
    if view.is_null() {
        return;
    }
    // SAFETY: Tauri's ns_view of a live window, used on the main thread.
    let view: &NSView = unsafe { &*(view as *const NSView) };
    let obj: Retained<AnyObject> = match item {
        ShareItem::File(p) => Retained::into_super(Retained::into_super(NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))),
        ShareItem::Url(u) => match NSURL::URLWithString(&NSString::from_str(&u)) {
            Some(u) => Retained::into_super(Retained::into_super(u)),
            None => return,
        },
        ShareItem::Text(t) => Retained::into_super(Retained::into_super(NSString::from_str(&t))),
    };
    let items = NSArray::from_retained_slice(&[obj]);
    // SAFETY: items are NSPasteboardWriting objects (NSURL / NSString).
    let picker = unsafe { NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items) };
    let b = view.bounds();
    let rect = NSRect::new(NSPoint::new(b.size.width / 2.0, b.size.height - 48.0), NSSize::new(1.0, 1.0));
    picker.showRelativeToRect_ofView_preferredEdge(rect, view, NSRectEdge::MinY);
    // the picker must outlive the menu it shows; one small object per share
    std::mem::forget(picker);
}

#[cfg(not(target_os = "macos"))]
fn show_share_picker(_view: *mut std::ffi::c_void, _item: ShareItem) {}

// ---------- drag and drop ----------

/// Shell command `files.drop { paths, zone? }` (native drop on a view; only the shell can send it).
/// Files are copied into `modules/<id>/data/inbox/` (files inside a granted folder are passed as /mnt/… paths);
/// returns file refs for the view (`openapp.onDrop` / the dropzone action gets `{ files }`).
fn files_drop(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = caller.module().map(String::from).or_else(|| s(p, "module").map(String::from)).ok_or("drop target is not a module view")?;
    let paths: Vec<PathBuf> = p.get("paths").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from).collect();
    if paths.is_empty() {
        return Err("files.drop needs \"paths\"".into());
    }
    let mut files = Vec::new();
    for src in paths.iter().take(100) {
        files.push(super::files::import(k, &module, src, "inbox").unwrap_or_else(|e| json!({ "name": src.file_name().map(|n| n.to_string_lossy().to_string()), "error": e })));
    }
    k.research.log("user", Kind::Drop, json!({ "module": module, "files": files.len() }));
    Ok(json!({ "ok": true, "files": files }))
}

fn drag_out(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let path = s(p, "path").ok_or("files.drag_out needs \"path\"")?;
    let host = crate::paths::map_any(k, path, true)?;
    let app = shell::app(k)?;
    let label = caller.placement.as_deref().map(super::shell_windows::window_label_for_placement).unwrap_or_else(|| "main".into());
    let w = app.get_webview_window(&label).or_else(|| app.get_webview_window("main")).ok_or("no window")?;
    let w2 = w.clone();
    w.run_on_main_thread(move || {
        let icon = drag::Image::Raw(include_bytes!("../../icons/32x32.png").to_vec());
        if let Err(e) = drag::start_drag(&w2, drag::DragItem::Files(vec![host]), icon, |_, _| {}, drag::Options::default()) {
            log::warn!("drag out: {e}");
        }
    })
    .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

// ---------- deep links and file handlers ----------

/// A URL macOS handed to OpenApp: `openapp://<module>/<path>?…` or a file (`file://…`).
pub async fn opened(k: &Arc<Kernel>, url: &str) -> Result<Value, String> {
    if let Some(rest) = url.strip_prefix("openapp://") {
        return deeplink(k, rest).await;
    }
    if url.starts_with("file://") {
        let u = url::Url::parse(url).map_err(|e| e.to_string())?;
        let path = u.to_file_path().map_err(|_| format!("bad file URL {url}"))?;
        return open_file(k, &path).await;
    }
    Err(format!("OpenApp cannot open {url}"))
}

/// Manifest `"deeplinks": ["prefix" | { "prefix", "action" }]`; default action `{ "tool": "on_deeplink" }`
/// with `{ url, path, query }`. Every deep link is also published on `kernel/deeplink`.
pub async fn deeplink(k: &Arc<Kernel>, rest: &str) -> Result<Value, String> {
    let (target, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (module, path) = target.split_once('/').unwrap_or((target, ""));
    let query_obj: Map<String, Value> = url::form_urlencoded::parse(query.as_bytes()).map(|(a, b)| (a.to_string(), json!(b))).collect();
    let url = format!("openapp://{rest}");
    let msg = json!({ "url": url, "module": module, "path": path, "query": query_obj });
    let _ = shell::publish(k, "kernel", "kernel/deeplink", msg.clone());
    let manifest = k.lock().modules.get(module).and_then(|m| m.manifest.as_ref().map(|mf| mf.raw.clone()));
    let Some(raw) = manifest else {
        return Err(format!("openapp:// link for unknown module \"{module}\""));
    };
    let handlers = raw.get("deeplinks").and_then(Value::as_array).cloned().unwrap_or_default();
    let hit = handlers.iter().find_map(|h| {
        let (prefix, action) = match h {
            Value::String(p) => (p.as_str(), None),
            Value::Object(o) => (o.get("prefix").and_then(Value::as_str).unwrap_or(""), o.get("action").cloned()),
            _ => return None,
        };
        let p = prefix.trim_matches('/');
        (p.is_empty() || path == p || path.starts_with(&format!("{p}/"))).then(|| action.unwrap_or_else(|| json!({ "tool": "on_deeplink" })))
    });
    let action = hit.ok_or_else(|| format!("module \"{module}\" does not handle openapp://{module}/{path} (manifest \"deeplinks\")"))?;
    let _ = show_main(k);
    k.research.log("user", Kind::Deeplink, json!({ "module": module, "path": path }));
    let r = shell::run_action(k, "deeplink", module, &action, json!({ "url": url, "path": path, "query": query_obj })).await?;
    Ok(json!({ "ok": true, "module": module, "result": r }))
}

/// Manifest `"opens": [".eml", "audio/*", { "types": [...], "action" }]`; default action `{ "tool": "on_open" }`
/// with `{ file: ref }`. The file is copied into the module's `data/inbox/` (or passed as /mnt/… inside a grant).
pub async fn open_file(k: &Arc<Kernel>, path: &Path) -> Result<Value, String> {
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase())).unwrap_or_default();
    let mime = super::files::mime_of(path);
    let mods: Vec<(String, Value)> = k.lock().modules.iter().filter_map(|(id, m)| m.manifest.as_ref().map(|mf| (id.clone(), mf.raw.clone()))).collect();
    let mut found = None;
    'outer: for (id, raw) in &mods {
        for h in raw.get("opens").and_then(Value::as_array).into_iter().flatten() {
            let (types, action): (Vec<String>, Option<Value>) = match h {
                Value::String(t) => (vec![t.clone()], None),
                Value::Object(o) => (o.get("types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect(), o.get("action").cloned()),
                _ => continue,
            };
            if types.iter().any(|t| type_matches(t, &ext, &mime)) {
                found = Some((id.clone(), action.unwrap_or_else(|| json!({ "tool": "on_open" }))));
                break 'outer;
            }
        }
    }
    let (module, action) = found.ok_or_else(|| format!("no module opens {} files (manifest \"opens\")", if ext.is_empty() { mime.clone() } else { ext.clone() }))?;
    let file = super::files::import(k, &module, path, "inbox")?;
    let _ = show_main(k);
    k.research.log("user", Kind::OpenFile, json!({ "module": module, "ext": ext }));
    let r = shell::run_action(k, "opens", &module, &action, json!({ "file": file })).await?;
    Ok(json!({ "ok": true, "module": module, "file": file, "result": r }))
}

pub fn type_matches(t: &str, ext: &str, mime: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    if t.starts_with('.') {
        return t == ext;
    }
    if let Some(prefix) = t.strip_suffix("/*") {
        return mime.starts_with(&format!("{prefix}/"));
    }
    if t.contains('/') {
        return t == mime;
    }
    format!(".{t}") == ext
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_matching() {
        assert!(type_matches(".eml", ".eml", "message/rfc822"));
        assert!(type_matches("eml", ".eml", "message/rfc822"));
        assert!(type_matches("audio/*", ".mp3", "audio/mpeg"));
        assert!(!type_matches("audio/*", ".png", "image/png"));
        assert!(type_matches("image/png", ".png", "image/png"));
    }

    #[test]
    fn png_encoder() {
        let png = encode_png(&[255, 0, 0, 255, 0, 255, 0, 128], 2, 1).unwrap();
        assert!(tauri::image::Image::from_bytes(&png).is_ok());
    }
}
