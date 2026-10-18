//! §21 G5 files: native open/save panels, folder grants (security-scoped bookmarks in `<state>/folders.json`,
//! mounted into the app VM at `/mnt/<name>`, readable by host services/modules through `/mnt/<name>/…` paths),
//! reveal in Finder, Quick Look.
//!
//! File refs given to modules: `{ path, name, size, mime }` where `path` is a VM path (`/workspace/…` for copies in
//! the module's `data/`, `/mnt/<grant>/…` inside a granted folder) that host services also accept.

use crate::research::Kind;
use crate::util::MutexExt;
use super::shell::{self, s};
use super::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn primitive() -> Primitive {
    let open = handler(|k, p, c| async move { files_open(&k, &p, &c).await });
    let save = handler(|k, p, c| async move { files_save(&k, &p, &c).await });
    let grant = handler(|k, p, c| async move { grant_folder(&k, &p, &c).await });
    let revoke = handler(|k, p, c| async move { revoke_folder(&k, &p, &c).await });
    let reveal = handler(|k, p, _c| async move { reveal(&k, &p).await });
    let ql = handler(|k, p, _c| async move { quicklook(&k, &p) });
    Primitive {
        name: "files",
        purpose: "The user's files: open/save panels, folder grants mounted in the VM at /mnt/<name>, reveal in Finder, Quick Look",
        example: r#"{ "host": "files.grant_folder", "purpose": "Save your songs", "name": "music" } → /mnt/music in the VM · { "host": "files.save", "suggestedName": "mix.wav", "path": "/workspace/modules/daw/data/mix.wav" }"#,
        class: Class::Granted,
        actions: vec![
            Action { name: "files.open", args: "{ types?: [\".md\", \"image/*\"], multiple?, directory?, title? } → { files: [ref] } (copies into the module's data/inbox; a folder becomes a grant)", handler: open.clone() },
            Action { name: "files.save", args: "{ suggestedName, types?, text | b64 | path, title? } → { path (on the Mac), guest? }", handler: save.clone() },
            Action { name: "files.grant_folder", args: "{ purpose, name? } → { folder: { name, path, guest: \"/mnt/<name>\" } } (panel; the VM restarts in ~1 s to mount it)", handler: grant.clone() },
            Action { name: "files.revoke_folder", args: "{ name }", handler: revoke.clone() },
            Action { name: "files.reveal", args: "{ path } (Finder)", handler: reveal.clone() },
            Action { name: "files.quicklook", args: "{ path }", handler: ql.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "files.open", args: "{ types?, multiple?, directory? }", handler: open },
            Endpoint { method: "files.save", args: "{ suggestedName, types?, text | b64 | path }", handler: save },
            Endpoint { method: "files.grant_folder", args: "{ purpose, name? }", handler: grant },
            Endpoint { method: "files.revoke_folder", args: "{ name }", handler: revoke },
            Endpoint { method: "files.reveal", args: "{ path }", handler: reveal },
            Endpoint { method: "files.quicklook", args: "{ path }", handler: ql },
        ],
        data: vec![DataSource {
            name: "folders",
            doc: "granted folders: [{ id, title, subtitle, icon, name, path, guest, purpose, module, action }] (action revokes)",
            get: data_fn(|k| {
                Value::Array(
                    folders(k)
                        .into_iter()
                        .map(|f| {
                            let name = f["name"].as_str().unwrap_or("").to_string();
                            json!({ "id": name, "title": f["path"], "subtitle": format!("/mnt/{name} · {}", f["purpose"].as_str().unwrap_or("")), "icon": "folder", "name": name, "path": f["path"], "guest": format!("/mnt/{name}"), "purpose": f["purpose"], "module": f["module"], "action": { "host": "files.revoke_folder", "name": name } })
                        })
                        .collect(),
                )
            }),
        }],
        ..Default::default()
    }
}

// ---------- folder store ----------

fn store_path_for(state_dir: &Path) -> PathBuf {
    state_dir.join("folders.json")
}

fn read_store(state_dir: &Path) -> Vec<Value> {
    std::fs::read(store_path_for(state_dir)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

/// Serializes read-modify-write of `folders.json` (writes are atomic too).
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_store(state_dir: &Path, v: &[Value]) {
    if let Err(e) = crate::state::write_atomic(&store_path_for(state_dir), &serde_json::to_vec_pretty(v).unwrap_or_default()) {
        log::warn!("folders.json: {e}");
    }
}

/// Granted folders with their current path (bookmarks follow moves and renames).
pub fn folders(k: &Kernel) -> Vec<Value> {
    resolved(&k.state_dir)
}

pub(crate) fn resolved(state_dir: &Path) -> Vec<Value> {
    let _l = STORE_LOCK.locked();
    let mut list = read_store(state_dir);
    let mut changed = false;
    for f in &mut list {
        let Some(b) = f["bookmark"].as_str().and_then(crate::kernel::base64_decode) else { continue };
        if let Ok((p, stale)) = bookmark::resolve(&b) {
            let ps = p.to_string_lossy().to_string();
            if f["path"] != ps.as_str() {
                f["path"] = json!(ps);
                changed = true;
            }
            if stale && let Ok(nb) = bookmark::create(&p) {
                f["bookmark"] = json!(b64(&nb));
                changed = true;
            }
        }
    }
    if changed {
        write_store(state_dir, &list);
    }
    list
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// `(guest path, host dir)` bind mounts for the app VM (read at VM create by vm.rs).
pub fn vm_mounts(host_ws: &Path) -> Vec<(String, PathBuf)> {
    let state_dir = crate::state::state_dir(host_ws);
    resolved(&state_dir)
        .into_iter()
        .filter_map(|f| {
            let name = f["name"].as_str()?.to_string();
            let p = PathBuf::from(f["path"].as_str()?);
            p.is_dir().then(|| (format!("/mnt/{name}"), p))
        })
        .collect()
}

/// Host data `grants` entries (`folder:<name>`).
pub fn grants_list(k: &Kernel) -> Vec<Value> {
    folders(k)
        .into_iter()
        .map(|f| json!({ "id": format!("folder:{}", f["name"].as_str().unwrap_or("")), "kind": "folder", "target": f["path"], "allowed": true, "scope": "workspace", "ts": f["ts"], "guest": format!("/mnt/{}", f["name"].as_str().unwrap_or("")) }))
        .collect()
}

fn sanitize_name(s: &str) -> String {
    let mut out: String = s.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').chars().take(32).collect::<String>();
    if out.is_empty() { "folder".into() } else { out }
}

/// Grant a folder (after the panel, or programmatically in tests). Returns the folder entry.
pub async fn grant_path(k: &Arc<Kernel>, path: &Path, name: Option<&str>, purpose: &str, module: Option<&str>, actor: &str) -> Result<Value, String> {
    let path = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("{} is not a folder", path.display()));
    }
    let ws = k.workspace.canonicalize().unwrap_or_else(|_| k.workspace.clone());
    if path.starts_with(&ws) {
        return Err("that folder is inside the workspace (already at /workspace)".into());
    }
    if path == Path::new("/") || dirs::home_dir().is_some_and(|h| path == h) {
        return Err("grant a specific folder, not the whole disk or home folder".into());
    }
    let bm = bookmark::create(&path)?;
    let l = STORE_LOCK.locked();
    let mut list = read_store(&k.state_dir);
    let ps = path.to_string_lossy().to_string();
    let existing = list.iter().position(|f| f["path"] == ps.as_str());
    let base = sanitize_name(name.unwrap_or_else(|| path.file_name().and_then(|n| n.to_str()).unwrap_or("folder")));
    let name = match existing {
        Some(i) => list[i]["name"].as_str().unwrap_or(&base).to_string(),
        None => {
            let mut n = base.clone();
            let mut i = 2;
            while list.iter().any(|f| f["name"] == n.as_str()) {
                n = format!("{base}-{i}");
                i += 1;
            }
            n
        }
    };
    let entry = json!({ "name": name, "path": ps, "bookmark": b64(&bm), "purpose": purpose, "module": module, "ts": crate::state::now_ts() });
    match existing {
        Some(i) => list[i] = entry.clone(),
        None => list.push(entry.clone()),
    }
    write_store(&k.state_dir, &list);
    drop(l);
    k.research.log(actor, Kind::FolderGrant, json!({ "name": name, "module": module, "purpose": purpose }));
    k.emit_snapshot();
    let remount = if existing.is_none() { remount(k).await } else { json!({ "restarted": false, "reason": "already mounted" }) };
    let mut out = json!({ "ok": true, "folder": { "name": name, "path": ps, "guest": format!("/mnt/{name}"), "purpose": purpose }, "vm": remount });
    out["folder"]["module"] = json!(module);
    Ok(out)
}

pub async fn revoke_name(k: &Arc<Kernel>, name: &str, actor: &str) -> Result<Value, String> {
    {
        let _l = STORE_LOCK.locked();
        let mut list = read_store(&k.state_dir);
        let before = list.len();
        list.retain(|f| f["name"] != name);
        if list.len() == before {
            return Err(format!("no granted folder \"{name}\""));
        }
        write_store(&k.state_dir, &list);
    }
    k.research.log(actor, Kind::FolderRevoke, json!({ "name": name }));
    k.emit_snapshot();
    let vm = remount(k).await;
    Ok(json!({ "ok": true, "revoked": name, "vm": vm }))
}

/// Mounts are fixed at VM create: restart the running VM (≈ 1 s) and reload the modules that ran in it.
/// Waits for running agent turns first (≤ 10 min); a VM harness is restarted too.
pub async fn remount(k: &Arc<Kernel>) -> Value {
    if k.vm().is_none() {
        return json!({ "restarted": false, "reason": "the app VM is not running; mounts apply when it starts" });
    }
    let t_wait = Instant::now();
    while k.working.load(std::sync::atomic::Ordering::Relaxed) > 0 && t_wait.elapsed().as_secs() < 600 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let t0 = Instant::now();
    let in_vm: Vec<String> = k.lock().modules.values().filter(|m| m.in_vm && m.client.is_some()).map(|m| m.id.clone()).collect();
    let old = k.vm.locked().take();
    if let Some(vm) = old {
        vm.stop().await;
    }
    let stop_ms = t0.elapsed().as_millis() as u64;
    let r = k.ensure_vm().await;
    let boot_ms = t0.elapsed().as_millis() as u64 - stop_ms;
    if let Err(e) = r {
        return json!({ "restarted": false, "error": e });
    }
    for id in &in_vm {
        let _ = k.module_load(id).await;
    }
    if k.vm_required() {
        crate::harness::restart(k);
    }
    k.research.log("kernel", Kind::VmRemount, json!({ "stop_ms": stop_ms, "boot_ms": boot_ms, "modules": in_vm.len() }));
    json!({ "restarted": true, "stop_ms": stop_ms, "boot_ms": boot_ms, "modules_reloaded": in_vm })
}

/// Host path → `/mnt/<grant>/…` when inside a granted folder.
pub fn guest_of(k: &Kernel, host: &Path) -> Option<String> {
    let host = host.canonicalize().ok()?;
    for f in folders(k) {
        let root = PathBuf::from(f["path"].as_str()?).canonicalize().ok()?;
        if let Ok(rel) = host.strip_prefix(&root) {
            let r = rel.to_string_lossy();
            return Some(if r.is_empty() { format!("/mnt/{}", f["name"].as_str()?) } else { format!("/mnt/{}/{r}", f["name"].as_str()?) });
        }
    }
    let ws = k.workspace.canonicalize().ok()?;
    host.strip_prefix(&ws).ok().map(|r| format!("{}/{}", crate::vm::GUEST_WS, r.to_string_lossy()))
}

pub fn mime_of(p: &Path) -> String {
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "json" => "application/json",
        "js" | "mjs" | "ts" => "text/javascript",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "eml" => "message/rfc822",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "mid" | "midi" => "audio/midi",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn ref_of(path_for_module: String, host: &Path) -> Value {
    let size = std::fs::metadata(host).map(|m| m.len()).unwrap_or(0);
    json!({ "path": path_for_module, "name": host.file_name().map(|n| n.to_string_lossy().to_string()), "size": size, "mime": mime_of(host) })
}

/// Give a module a user-chosen file: inside a grant → its /mnt path; else a copy (APFS clone) in
/// `modules/<module>/data/<sub>/` (unique name). Folders are refused (use a folder grant).
pub fn import(k: &Kernel, module: &str, src: &Path, sub: &str) -> Result<Value, String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if meta.is_dir() {
        return Err("folders are not copied; use files.grant_folder".into());
    }
    if let Some(g) = guest_of(k, src) {
        return Ok(ref_of(g, src));
    }
    let dir = k.workspace.join("modules").join(module).join("data").join(sub);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((a, b)) if !a.is_empty() => (a.to_string(), format!(".{b}")),
        _ => (name.clone(), String::new()),
    };
    let mut dest = dir.join(&name);
    let mut i = 2;
    while dest.exists() {
        dest = dir.join(format!("{stem} ({i}){ext}"));
        i += 1;
    }
    std::fs::copy(src, &dest).map_err(|e| format!("copy {}: {e}", src.display()))?;
    let rel = dest.strip_prefix(&k.workspace).map(|r| r.to_string_lossy().to_string()).unwrap_or_default();
    Ok(ref_of(format!("{}/{rel}", crate::vm::GUEST_WS), &dest))
}

// ---------- panels ----------

/// `types` → dialog filter extensions (".md", "md", "image/*", "audio/mpeg").
pub fn extensions(types: &[String]) -> Vec<String> {
    const GROUPS: &[(&str, &[&str])] = &[
        ("image", &["png", "jpg", "jpeg", "gif", "webp", "heic", "tiff", "svg"]),
        ("audio", &["mp3", "m4a", "wav", "aiff", "aif", "flac", "ogg", "mid", "midi", "aac"]),
        ("video", &["mp4", "mov", "m4v", "webm", "mkv"]),
        ("text", &["txt", "md", "csv", "html", "json", "log"]),
    ];
    let mut out: Vec<String> = Vec::new();
    for t in types {
        let t = t.trim().to_ascii_lowercase();
        if let Some(g) = t.strip_suffix("/*") {
            if let Some((_, exts)) = GROUPS.iter().find(|(n, _)| *n == g) {
                out.extend(exts.iter().map(|e| e.to_string()));
            }
        } else if t.contains('/') {
            // a mime: every extension that maps to it
            let all = ["txt", "md", "html", "css", "csv", "json", "xml", "pdf", "zip", "eml", "ics", "vcf", "png", "jpg", "gif", "webp", "heic", "svg", "tiff", "mp3", "m4a", "wav", "aiff", "flac", "ogg", "mid", "mp4", "mov", "webm", "mkv"];
            out.extend(all.iter().filter(|e| mime_of(Path::new(&format!("x.{e}"))) == t).map(|e| e.to_string()));
        } else {
            out.push(t.trim_start_matches('.').to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn types_of(p: &Value) -> Vec<String> {
    p.get("types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
}

fn dialog(k: &Kernel, p: &Value) -> Result<tauri_plugin_dialog::FileDialogBuilder<tauri::Wry>, String> {
    use tauri_plugin_dialog::DialogExt;
    let app = shell::app(k)?;
    let mut d = app.dialog().file();
    let exts = extensions(&types_of(p));
    if !exts.is_empty() {
        let refs: Vec<&str> = exts.iter().map(String::as_str).collect();
        d = d.add_filter("Files", &refs);
    }
    if let Some(t) = s(p, "title") {
        d = d.set_title(t);
    }
    Ok(d)
}

async fn files_open(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p)?;
    let d = dialog(k, p)?;
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<PathBuf>>();
    if p.get("directory").and_then(Value::as_bool).unwrap_or(false) {
        d.pick_folder(move |f| {
            let _ = tx.send(f.and_then(|f| f.into_path().ok()).into_iter().collect());
        });
        let picked = rx.await.unwrap_or_default();
        let Some(dir) = picked.into_iter().next() else { return Ok(json!({ "ok": true, "cancelled": true, "files": [] })) };
        let purpose = s(p, "purpose").or_else(|| s(p, "title")).unwrap_or("opened as a folder");
        return grant_path(k, &dir, s(p, "name"), purpose, Some(&module), "user").await;
    }
    if p.get("multiple").and_then(Value::as_bool).unwrap_or(false) {
        d.pick_files(move |f| {
            let _ = tx.send(f.unwrap_or_default().into_iter().filter_map(|f| f.into_path().ok()).collect());
        });
    } else {
        d.pick_file(move |f| {
            let _ = tx.send(f.and_then(|f| f.into_path().ok()).into_iter().collect());
        });
    }
    let picked = rx.await.unwrap_or_default();
    if picked.is_empty() {
        return Ok(json!({ "ok": true, "cancelled": true, "files": [] }));
    }
    let files: Vec<Value> = picked.iter().map(|f| import(k, &module, f, "inbox").unwrap_or_else(|e| json!({ "error": e }))).collect();
    k.research.log(&caller.id, Kind::FilesOpen, json!({ "module": module, "files": files.len() }));
    Ok(json!({ "ok": true, "files": files }))
}

async fn files_save(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p)?;
    let data: Vec<u8> = if let Some(t) = p.get("text").and_then(Value::as_str) {
        t.as_bytes().to_vec()
    } else if let Some(b) = s(p, "b64") {
        crate::kernel::base64_decode(b).ok_or("bad base64 in \"b64\"")?
    } else if let Some(src) = s(p, "path") {
        std::fs::read(crate::paths::map_any(k, src, true)?).map_err(|e| e.to_string())?
    } else {
        return Err("files.save needs \"text\", \"b64\" or \"path\" (the file to save)".into());
    };
    let mut d = dialog(k, p)?.set_can_create_directories(true);
    if let Some(n) = s(p, "suggestedName") {
        d = d.set_file_name(n);
    }
    if let Some(dir) = s(p, "directory") {
        // a granted folder (/mnt/<name>) or a host folder name like "~/Music"
        let host = if dir.starts_with("/mnt/") { crate::paths::map_any(k, dir, true).ok() } else { dirs::home_dir().map(|h| h.join(dir.trim_start_matches("~/"))) };
        if let Some(h) = host.filter(|h| h.is_dir()) {
            d = d.set_directory(h);
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    d.save_file(move |f| {
        let _ = tx.send(f.and_then(|f| f.into_path().ok()));
    });
    let Some(dest) = rx.await.ok().flatten() else { return Ok(json!({ "ok": true, "cancelled": true })) };
    std::fs::write(&dest, &data).map_err(|e| format!("could not write {}: {e}", dest.display()))?;
    k.research.log(&caller.id, Kind::FilesSave, json!({ "module": module, "bytes": data.len() }));
    Ok(json!({ "ok": true, "path": dest.to_string_lossy(), "name": dest.file_name().map(|n| n.to_string_lossy().to_string()), "guest": guest_of(k, &dest), "size": data.len() }))
}

async fn grant_folder(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let module = shell::owner(k, caller, p).ok();
    let purpose = s(p, "purpose").ok_or("files.grant_folder needs \"purpose\" (one sentence the user sees, e.g. \"Save your songs\")")?.to_string();
    let who = module.as_deref().map(|m| k.lock().modules.get(m).map(|e| e.name.clone()).unwrap_or_else(|| m.to_string())).unwrap_or_else(|| "OpenApp".into());
    let app = shell::app(k)?;
    let mut d = app.dialog().file().set_title(format!("{who}: {purpose}")).set_can_create_directories(true);
    if let Some(dir) = s(p, "suggested")
        && let Some(h) = dirs::home_dir().map(|h| h.join(dir.trim_start_matches("~/")))
        && h.is_dir()
    {
        d = d.set_directory(h);
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    d.pick_folder(move |f| {
        let _ = tx.send(f.and_then(|f| f.into_path().ok()));
    });
    let Some(dir) = rx.await.ok().flatten() else { return Ok(json!({ "ok": true, "cancelled": true })) };
    grant_path(k, &dir, s(p, "name"), &purpose, module.as_deref(), &caller.id).await
}

async fn revoke_folder(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let name = s(p, "name").or_else(|| s(p, "id")).ok_or("files.revoke_folder needs \"name\"")?;
    revoke_name(k, name.trim_start_matches("folder:"), &caller.id).await
}

async fn reveal(k: &Arc<Kernel>, p: &Value) -> Result<Value, String> {
    let host = crate::paths::map_any(k, s(p, "path").ok_or("files.reveal needs \"path\"")?, true)?;
    let st = tokio::process::Command::new("open").arg("-R").arg(&host).status().await.map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("Finder could not reveal the file".into());
    }
    Ok(json!({ "ok": true }))
}

fn quicklook(k: &Arc<Kernel>, p: &Value) -> Result<Value, String> {
    let host = crate::paths::map_any(k, s(p, "path").ok_or("files.quicklook needs \"path\"")?, true)?;
    std::process::Command::new("qlmanage")
        .arg("-p")
        .arg(&host)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Quick Look: {e}"))?;
    Ok(json!({ "ok": true }))
}

// ---------- security-scoped bookmarks ----------

#[cfg(target_os = "macos")]
pub mod bookmark {
    use objc2::rc::Retained;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSData, NSString, NSURL, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions};
    use std::path::{Path, PathBuf};

    /// Security-scoped bookmark data for a folder (falls back to a plain bookmark if the scope is refused).
    pub fn create(path: &Path) -> Result<Vec<u8>, String> {
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), true);
        let data = url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(NSURLBookmarkCreationOptions::WithSecurityScope, None, None)
            .or_else(|_| url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(NSURLBookmarkCreationOptions(0), None, None))
            .map_err(|e| format!("bookmark: {}", e.localizedDescription()))?;
        Ok(data.to_vec())
    }

    /// Resolve bookmark data → (current path, stale). Starts the security scope for this process.
    pub fn resolve(data: &[u8]) -> Result<(PathBuf, bool), String> {
        let d = NSData::with_bytes(data);
        let mut stale = Bool::NO;
        let try_opts = [NSURLBookmarkResolutionOptions::WithSecurityScope, NSURLBookmarkResolutionOptions(0)];
        let mut last = String::new();
        for o in try_opts {
            // SAFETY: `stale` is a valid pointer for the call.
            let r: Result<Retained<NSURL>, _> = unsafe { NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(&d, o, None, &mut stale) };
            match r {
                Ok(url) => {
                    // SAFETY: plain message send on a file URL.
                    unsafe {
                        let _ = url.startAccessingSecurityScopedResource();
                    }
                    let p = url.path().map(|s| s.to_string()).ok_or("bookmark has no path")?;
                    return Ok((PathBuf::from(p), stale.as_bool()));
                }
                Err(e) => last = e.localizedDescription().to_string(),
            }
        }
        Err(format!("bookmark: {last}"))
    }
}

#[cfg(not(target_os = "macos"))]
pub mod bookmark {
    use std::path::{Path, PathBuf};
    pub fn create(path: &Path) -> Result<Vec<u8>, String> {
        Ok(path.to_string_lossy().as_bytes().to_vec())
    }
    pub fn resolve(data: &[u8]) -> Result<(PathBuf, bool), String> {
        Ok((PathBuf::from(String::from_utf8_lossy(data).to_string()), false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_types() {
        assert_eq!(sanitize_name("My Music!"), "my-music");
        assert_eq!(sanitize_name("***"), "folder");
        assert!(extensions(&[".MD".into(), "image/*".into()]).contains(&"md".to_string()));
        assert!(extensions(&["audio/mpeg".into()]).contains(&"mp3".to_string()));
        assert_eq!(mime_of(Path::new("a.eml")), "message/rfc822");
    }

    #[test]
    fn bookmark_roundtrip() {
        let d = std::env::temp_dir().join(format!("oa-bm-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let b = bookmark::create(&d).unwrap();
        let (p, _) = bookmark::resolve(&b).unwrap();
        assert_eq!(p.canonicalize().unwrap(), d.canonicalize().unwrap());
        // follows a rename
        let d2 = d.with_file_name(format!("oa-bm-{}-moved", std::process::id()));
        std::fs::rename(&d, &d2).unwrap();
        let (p2, _) = bookmark::resolve(&b).unwrap();
        assert_eq!(p2.canonicalize().unwrap(), d2.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&d2);
    }
}
