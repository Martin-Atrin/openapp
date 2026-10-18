//! Host modules (contract §18.1): agent-written Swift that the kernel compiles and runs on the Mac.
//! - Sources: `host.swift` (all `.swift` files in that file's folder) or `host.package` (a SwiftPM folder).
//! - Consent per source hash: a kernel card (summary, imported frameworks, shell/AppleScript use, diff
//!   vs the last approved source). The authoritative approval record and the approved source snapshot
//!   live OUTSIDE the workspace (`~/.openapp/host-build/<module>/<hash>/`), so the agent cannot approve
//!   its own code by editing files; `<state>/grants.json` mirrors the decision for the user.
//! - The kernel compiles the approved snapshot (not the live workspace files) with `swiftc` / SwiftPM.

use crate::modules::HostSpec;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Sources {
    /// path relative to the module folder → content
    pub files: BTreeMap<String, String>,
    pub hash: String,
    /// relative path of the SwiftPM folder (package mode)
    pub package: Option<String>,
}

pub fn build_root() -> PathBuf {
    match std::env::var_os("OPENAPP_HOST_BUILD").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".openapp").join("host-build"),
    }
}

fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>, depth: usize, swift_only: bool) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            if depth < 6 && name != "data" && name != "node_modules" {
                walk(&p, base, out, depth + 1, swift_only)?;
            }
        } else if !swift_only || name.ends_with(".swift") {
            if name.ends_with(".swift") || name == "Package.resolved" || name.ends_with(".json") || name.ends_with(".plist") {
                let text = std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
                out.insert(p.strip_prefix(base).unwrap_or(&p).to_string_lossy().to_string(), text);
            }
            if out.len() > 200 {
                return Err("host part has too many files (max 200)".into());
            }
        }
    }
    Ok(())
}

/// Collect the host sources of a module folder.
pub fn collect(dir: &Path, spec: &HostSpec) -> Result<Sources, String> {
    let mut files = BTreeMap::new();
    let package = spec.package.clone();
    match (&spec.swift, &package) {
        (Some(f), _) => {
            let main = dir.join(f);
            if !main.is_file() {
                return Err(format!("host.swift file {f} not found"));
            }
            let folder = main.parent().unwrap_or(dir);
            for e in std::fs::read_dir(folder).map_err(|e| e.to_string())?.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "swift") && p.is_file() {
                    let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
                    files.insert(p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().to_string(), text);
                }
            }
        }
        (None, Some(pkg)) => {
            let root = dir.join(pkg);
            if !root.join("Package.swift").is_file() {
                return Err(format!("host.package folder {pkg} has no Package.swift"));
            }
            walk(&root, dir, &mut files, 0, false)?;
        }
        _ => return Err("host needs \"swift\" or \"package\"".into()),
    }
    if files.is_empty() {
        return Err("no Swift sources found for the host part".into());
    }
    let mut h = Sha256::new();
    for (k, v) in &files {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    let hash = h.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect();
    Ok(Sources { files, hash, package })
}

/// Frameworks whose first use can make macOS show a privacy (TCC) prompt, which blocks the call until the user answers.
pub const TCC_FRAMEWORKS: &[&str] = &[
    "EventKit",
    "Contacts",
    "Photos",
    "PhotosUI",
    "AVFoundation",
    "AVFAudio",
    "CoreLocation",
    "CoreBluetooth",
    "Speech",
    "HomeKit",
    "ScreenCaptureKit",
    "CoreWLAN",
    "UserNotifications",
    "LocalAuthentication",
    "Intents",
    "MediaPlayer",
    "iTunesLibrary",
    "ApplicationServices",
];

/// The [`TCC_FRAMEWORKS`] a host module's sources import.
pub fn tcc_frameworks(files: &BTreeMap<String, String>) -> Vec<String> {
    scan(files).0.into_iter().filter(|m| TCC_FRAMEWORKS.contains(&m.as_str())).collect()
}

/// Imported frameworks and powerful calls, for the consent card (simple text scan).
pub fn scan(files: &BTreeMap<String, String>) -> (Vec<String>, Vec<String>) {
    let mut imports = std::collections::BTreeSet::new();
    let mut calls = std::collections::BTreeSet::new();
    for text in files.values() {
        for line in text.lines() {
            let t = line.trim();
            let t = t.strip_prefix("@preconcurrency ").or_else(|| t.strip_prefix("@_exported ")).unwrap_or(t);
            if let Some(rest) = t.strip_prefix("import ") {
                let m = rest.split_whitespace().last().unwrap_or("").trim_end_matches(';');
                if !m.is_empty() {
                    imports.insert(m.to_string());
                }
            }
        }
        let checks: &[(&[&str], &str)] = &[
            (&["Process(", "Process.run", "NSTask", "posix_spawn", "system("], "runs shell commands (Process)"),
            (&["NSAppleScript", "OSAScript"], "runs AppleScript"),
            (&["osascript"], "runs osascript"),
            (&["\"shortcuts\"", "/usr/bin/shortcuts", "shortcuts run"], "runs Shortcuts"),
            (&["URLSession", "NWConnection"], "uses the network"),
            (&["FileManager", "FileHandle(forWriting", ".write(to:", "removeItem"], "reads or writes files"),
            (&["CGEvent", "AXUIElement"], "controls the keyboard, mouse or other apps (Accessibility)"),
            (&["NSWindow", "NSPanel", "openWindow(", "NSHostingView", "requestViewController"], "opens its own windows"),
        ];
        for (needles, label) in checks {
            if needles.iter().any(|n| text.contains(n)) {
                calls.insert(label.to_string());
            }
        }
    }
    (imports.into_iter().collect(), calls.into_iter().collect())
}

/// Line diff of two source sets (unified-style, capped).
pub fn diff(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let names: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for n in names {
        let (a, b) = (old.get(n).map(String::as_str).unwrap_or(""), new.get(n).map(String::as_str).unwrap_or(""));
        if a == b {
            continue;
        }
        out.push_str(&format!("--- {}\n+++ {}\n", if old.contains_key(n) { n.as_str() } else { "/dev/null" }, if new.contains_key(n) { n.as_str() } else { "/dev/null" }));
        let (al, bl): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
        if al.len() * bl.len() > 4_000_000 {
            out.push_str("(file too large for a line diff; replaced)\n");
            continue;
        }
        // LCS table
        let (n1, n2) = (al.len(), bl.len());
        let mut t = vec![vec![0u32; n2 + 1]; n1 + 1];
        for i in (0..n1).rev() {
            for j in (0..n2).rev() {
                t[i][j] = if al[i] == bl[j] { t[i + 1][j + 1] + 1 } else { t[i + 1][j].max(t[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n1 || j < n2 {
            if i < n1 && j < n2 && al[i] == bl[j] {
                i += 1;
                j += 1;
            } else if j < n2 && (i == n1 || t[i][j + 1] >= t[i + 1][j]) {
                out.push_str(&format!("+{}\n", bl[j]));
                j += 1;
            } else {
                out.push_str(&format!("-{}\n", al[i]));
                i += 1;
            }
        }
        if out.len() > 60_000 {
            out.push_str("… (diff truncated)\n");
            break;
        }
    }
    out
}

pub fn module_root(module: &str) -> PathBuf {
    build_root().join(module)
}

pub fn hash_dir(module: &str, hash: &str) -> PathBuf {
    module_root(module).join(hash)
}

#[derive(Debug, PartialEq)]
pub enum Approval {
    Allowed,
    Denied,
    Unknown,
}

/// Decision for this exact source in this workspace (external record).
pub fn approval(module: &str, hash: &str, ws: &Path) -> Approval {
    let read = |name: &str| std::fs::read_to_string(hash_dir(module, hash).join(name)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let ws = ws.to_string_lossy().to_string();
    let has = |v: Option<Value>| v.and_then(|v| v["workspaces"].as_array().cloned()).is_some_and(|a| a.iter().any(|w| w == ws.as_str()));
    if has(read("denied.json")) {
        Approval::Denied
    } else if has(read("approved.json")) {
        Approval::Allowed
    } else {
        Approval::Unknown
    }
}

/// Persist a decision; an approval also snapshots the sources (compiled from there, diffed next time).
pub fn record(module: &str, src: &Sources, ws: &Path, allowed: bool) -> Result<(), String> {
    let dir = hash_dir(module, &src.hash);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = if allowed { "approved.json" } else { "denied.json" };
    let p = dir.join(name);
    let mut v: Value = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({ "workspaces": [] }));
    let ws = ws.to_string_lossy().to_string();
    if let Some(a) = v["workspaces"].as_array_mut()
        && !a.iter().any(|w| w == ws.as_str())
    {
        a.push(json!(ws));
    }
    v["ts"] = json!(crate::state::now_ts());
    std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap_or_default()).map_err(|e| e.to_string())?;
    if allowed {
        let srcdir = dir.join("src");
        let _ = std::fs::remove_dir_all(&srcdir);
        for (rel, text) in &src.files {
            let f = srcdir.join(rel);
            if let Some(parent) = f.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&f, text).map_err(|e| e.to_string())?;
        }
        let _ = std::fs::write(module_root(module).join("last-approved"), &src.hash);
    }
    Ok(())
}

/// Sources of the last approved build of a module (for the diff).
pub fn last_approved(module: &str) -> BTreeMap<String, String> {
    let Ok(hash) = std::fs::read_to_string(module_root(module).join("last-approved")) else { return BTreeMap::new() };
    let src = hash_dir(module, hash.trim()).join("src");
    let mut out = BTreeMap::new();
    let _ = walk(&src, &src, &mut out, 0, false);
    out
}

/// One compiler diagnostic, with the path relative to the workspace.
pub fn parse_diagnostics(text: &str, srcdir: &Path, module: &str) -> Vec<Value> {
    let prefix = format!("{}/", srcdir.to_string_lossy());
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(5, ':');
        let (Some(file), Some(l), Some(c), Some(sev), Some(msg)) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
        let sev = sev.trim();
        if !(sev == "error" || sev == "warning") || l.parse::<u32>().is_err() {
            continue;
        }
        let rel = file.strip_prefix(&prefix).map(|r| format!("modules/{module}/{r}")).unwrap_or_else(|| file.to_string());
        out.push(json!({ "file": rel, "line": l.parse::<u32>().unwrap_or(0), "column": c.parse::<u32>().unwrap_or(0), "severity": sev, "message": msg.trim() }));
        if out.len() >= 40 {
            break;
        }
    }
    out
}

/// The sources call the kernel's host-module runtime (`OpenAppHost.method(…)`, `OpenAppHost.run()`).
pub fn uses_runtime(files: &BTreeMap<String, String>) -> bool {
    files.iter().any(|(k, v)| k.ends_with(".swift") && v.contains("OpenAppHost.")) && !files.keys().any(|k| k.ends_with("OpenAppHost.swift"))
}

/// Short hash of the runtime source (part of the cached binary name, so a runtime change rebuilds).
fn runtime_rev() -> String {
    let h = Sha256::digest(crate::primitives::host_service::RUNTIME_SWIFT.as_bytes());
    h.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// §18.1 privacy usage descriptions: one list for the app's `Info.plist` (kept equal by a test) and the plist embedded
/// into every host-module binary (macOS reads the *executable's own* Info.plist; a host module without one is
/// killed with SIGABRT on its first Bluetooth / Contacts / … access).
pub const USAGE_DESCRIPTIONS: &[(&str, &str)] = &[
    ("NSMicrophoneUsageDescription", "OpenApp lets features you build use the microphone when you allow it."),
    ("NSCameraUsageDescription", "OpenApp lets features you build use the camera when you allow it."),
    ("NSContactsUsageDescription", "OpenApp lets features you build read your contacts when you allow it."),
    ("NSCalendarsUsageDescription", "OpenApp lets features you build use your calendars when you allow it."),
    ("NSCalendarsFullAccessUsageDescription", "OpenApp lets features you build use your calendars when you allow it."),
    ("NSRemindersUsageDescription", "OpenApp lets features you build use your reminders when you allow it."),
    ("NSRemindersFullAccessUsageDescription", "OpenApp lets features you build use your reminders when you allow it."),
    ("NSPhotoLibraryUsageDescription", "OpenApp lets features you build read your photos when you allow it."),
    ("NSPhotoLibraryAddUsageDescription", "OpenApp lets features you build add photos to your library when you allow it."),
    ("NSLocationWhenInUseUsageDescription", "OpenApp lets features you build use your location while you use the app."),
    ("NSLocationUsageDescription", "OpenApp lets features you build use your location while you use the app."),
    ("NSLocationAlwaysAndWhenInUseUsageDescription", "OpenApp lets features you build use your location when you allow it."),
    ("NSLocationAlwaysUsageDescription", "OpenApp lets features you build use your location when you allow it."),
    ("NSAppleEventsUsageDescription", "OpenApp lets features you build control other apps when you allow it."),
    ("NSBluetoothAlwaysUsageDescription", "OpenApp lets features you build use Bluetooth when you allow it."),
    ("NSBluetoothPeripheralUsageDescription", "OpenApp lets features you build use Bluetooth when you allow it."),
    ("NSSpeechRecognitionUsageDescription", "OpenApp transcribes your speech on this Mac when you allow it."),
    ("NSScreenCaptureUsageDescription", "OpenApp lets features you build capture the screen when you allow it."),
    ("NSDesktopFolderUsageDescription", "OpenApp lets features you build use files on your Desktop when you allow it."),
    ("NSDocumentsFolderUsageDescription", "OpenApp lets features you build use files in Documents when you allow it."),
    ("NSDownloadsFolderUsageDescription", "OpenApp lets features you build use files in Downloads when you allow it."),
    ("NSAppleMusicUsageDescription", "OpenApp lets features you build use your music library and what is playing when you allow it."),
    ("NSLocalNetworkUsageDescription", "OpenApp lets features you build find and talk to devices on your local network when you allow it."),
];

/// Bonjour service types host modules may browse on the LAN (`NSBonjourServices`).
pub const BONJOUR_SERVICES: &[&str] = &["_http._tcp", "_https._tcp", "_openapp._tcp", "_hap._tcp", "_home-assistant._tcp", "_mqtt._tcp", "_googlecast._tcp", "_airplay._tcp", "_raop._tcp", "_spotify-connect._tcp", "_ipp._tcp", "_printer._tcp", "_smb._tcp", "_ssh._tcp"];

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The Info.plist embedded into a host module's binary (`__TEXT,__info_plist`).
pub fn host_plist(module: &str) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    let id: String = module.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect();
    for (k, v) in [
        ("CFBundleIdentifier", format!("com.polygonface.openapp.host.{id}")),
        ("CFBundleName", xml_escape(module)),
        ("CFBundleExecutable", xml_escape(module)),
        ("CFBundleInfoDictionaryVersion", "6.0".into()),
        ("CFBundleVersion", "1".into()),
    ] {
        s.push_str(&format!("  <key>{k}</key>\n  <string>{v}</string>\n"));
    }
    for (k, v) in USAGE_DESCRIPTIONS {
        s.push_str(&format!("  <key>{k}</key>\n  <string>{}</string>\n", xml_escape(v)));
    }
    s.push_str("  <key>NSBonjourServices</key>\n  <array>\n");
    for b in BONJOUR_SERVICES {
        s.push_str(&format!("    <string>{b}</string>\n"));
    }
    s.push_str("  </array>\n</dict>\n</plist>\n");
    s
}

/// Short hash of [`host_plist`]'s content (part of the cached binary name: a new key rebuilds every module).
fn plist_rev() -> String {
    let h = Sha256::digest(host_plist("x").as_bytes());
    h.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Linker flags that embed `plist` as the binary's Info.plist.
fn plist_link_flags(plist: &Path) -> Vec<std::ffi::OsString> {
    let mut v: Vec<std::ffi::OsString> = Vec::new();
    for a in ["-sectcreate", "__TEXT", "__info_plist"] {
        v.push("-Xlinker".into());
        v.push(a.into());
    }
    v.push("-Xlinker".into());
    v.push(plist.as_os_str().to_os_string());
    v
}

/// The kernel runs inside an app bundle (`OpenApp.app/Contents/MacOS/…`): TCC then attributes a child's privacy
/// access to OpenApp.app, whose Info.plist carries [`USAGE_DESCRIPTIONS`].
pub fn in_app_bundle() -> bool {
    std::env::current_exe().ok().is_some_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// Make a host module responsible for itself (macOS `responsibility_spawnattrs_setdisclaim`), so TCC reads the
/// usage descriptions embedded in the module's own binary instead of the launching process's (a terminal, a test
/// runner or the probes harness, which lack them; macOS kills the module with SIGABRT then). Implemented as an
/// `exec` replacement: after fork, `posix_spawn(POSIX_SPAWN_SETEXEC)` with the disclaim attribute (same pid, the
/// piped stdio and env stay). Everything is prepared before fork; without the symbol the plain exec runs.
#[cfg(target_os = "macos")]
pub fn exec_responsible_for_itself(cmd: &mut tokio::process::Command, bin: &Path) {
    use std::os::unix::ffi::OsStrExt;
    type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
    struct Pre {
        attr: libc::posix_spawnattr_t,
        path: std::ffi::CString,
        argv: [*mut libc::c_char; 2],
    }
    // SAFETY: only read in the forked child (pre_exec); the pointers stay valid for the closure's lifetime
    unsafe impl Send for Pre {}
    unsafe impl Sync for Pre {}
    // SAFETY: dlsym with a NUL-terminated name; the symbol (libsystem, macOS ≥ 10.14) has this signature
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_spawnattrs_setdisclaim".as_ptr()) };
    let Ok(path) = std::ffi::CString::new(bin.as_os_str().as_bytes()) else { return };
    if sym.is_null() {
        return;
    }
    // SAFETY: non-null symbol of the known signature
    let set_disclaim: SetDisclaim = unsafe { std::mem::transmute::<*mut libc::c_void, SetDisclaim>(sym) };
    let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
    // SAFETY: attr is initialized by posix_spawnattr_init before use
    unsafe {
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return;
        }
        if libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short) != 0 || set_disclaim(&mut attr, 1) != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return;
        }
    }
    let argv0 = path.as_ptr() as *mut libc::c_char;
    let pre = Pre { attr, path, argv: [argv0, std::ptr::null_mut()] };
    // SAFETY: the closure runs in the forked child after stdio/cwd/env are set up; posix_spawn with SETEXEC
    // replaces the process image (it returns only on failure, then the regular exec follows)
    unsafe {
        cmd.pre_exec(move || {
            let p = &pre; // capture the whole (Send + Sync) struct, not its raw-pointer fields
            let mut pid: libc::pid_t = 0;
            let envp = *libc::_NSGetEnviron();
            libc::posix_spawn(&mut pid, p.path.as_ptr(), std::ptr::null(), &p.attr, p.argv.as_ptr(), envp as *const *mut libc::c_char);
            Ok(())
        });
    }
}

/// Compile the approved snapshot; returns the binary. Cached per hash.
pub async fn compile(module: &str, src: &Sources, spec: &HostSpec) -> Result<PathBuf, Value> {
    let dir = hash_dir(module, &src.hash);
    let srcdir = dir.join("src");
    let t0 = std::time::Instant::now();
    // the embedded Info.plist (usage descriptions) is written next to the build, linked into the binary
    let prev = plist_rev();
    let plist = dir.join(format!("Info-{prev}.plist"));
    let (bin, mut cmd) = match &src.package {
        None => {
            // §21 G8: sources that use `OpenAppHost` get the kernel runtime (AppKit run loop + JSON-RPC) compiled alongside
            let runtime = uses_runtime(&src.files);
            let bin = dir.join("bin").join(if runtime { format!("{module}-rt{}-p{prev}", runtime_rev()) } else { format!("{module}-p{prev}") });
            if bin.is_file() {
                return Ok(bin);
            }
            std::fs::create_dir_all(dir.join("bin")).map_err(|e| json!({ "error": e.to_string() }))?;
            std::fs::write(&plist, host_plist(module)).map_err(|e| json!({ "error": e.to_string() }))?;
            let mut c = tokio::process::Command::new("xcrun");
            c.args(["swiftc", "-O", "-module-name", "HostModule"]).args(plist_link_flags(&plist)).arg("-o").arg(&bin);
            for rel in src.files.keys().filter(|k| k.ends_with(".swift")) {
                c.arg(srcdir.join(rel));
            }
            if runtime {
                let rt = dir.join("runtime").join("OpenAppHost.swift");
                std::fs::create_dir_all(dir.join("runtime")).map_err(|e| json!({ "error": e.to_string() }))?;
                std::fs::write(&rt, crate::primitives::host_service::RUNTIME_SWIFT).map_err(|e| json!({ "error": e.to_string() }))?;
                c.arg(rt);
            }
            (bin, c)
        }
        Some(pkg) => {
            let product = spec.product.clone().unwrap_or_else(|| module.to_string());
            let scratch = dir.join(format!("spm-p{prev}"));
            let bin = scratch.join("release").join(&product);
            if bin.is_file() {
                return Ok(bin);
            }
            std::fs::write(&plist, host_plist(module)).map_err(|e| json!({ "error": e.to_string() }))?;
            let mut c = tokio::process::Command::new("swift");
            c.args(["build", "-c", "release", "--product", &product, "--package-path"]).arg(srcdir.join(pkg)).arg("--scratch-path").arg(&scratch);
            c.args(plist_link_flags(&plist));
            (bin, c)
        }
    };
    cmd.env_remove("SDKROOT").stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(std::time::Duration::from_secs(600), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(json!({ "error": format!("could not run the Swift compiler: {e}") })),
        Err(_) => return Err(json!({ "error": "the Swift build timed out after 600 s" })),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    if !out.status.success() || !bin.is_file() {
        let diags = parse_diagnostics(&text, &srcdir, module);
        let tail: Vec<&str> = text.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(json!({ "error": "the host part did not compile", "diagnostics": diags, "output_tail": if diags.is_empty() { tail.join("\n") } else { String::new() } }));
    }
    log::info!("host module {module} compiled in {} ms", t0.elapsed().as_millis());
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_diff_diags() {
        let mut f = BTreeMap::new();
        f.insert("host/main.swift".to_string(), "import Foundation\n@preconcurrency import EventKit\nlet p = Process()\nlet s = NSAppleScript(source: \"\")\n".to_string());
        let (i, c) = scan(&f);
        assert_eq!(i, vec!["EventKit", "Foundation"]);
        assert_eq!(tcc_frameworks(&f), vec!["EventKit"], "privacy-prompt frameworks only");
        assert!(c.iter().any(|x| x.contains("Process")) && c.iter().any(|x| x.contains("AppleScript")));
        let mut g = f.clone();
        g.insert("host/main.swift".to_string(), "import Foundation\nprint(1)\n".to_string());
        let d = diff(&f, &g);
        assert!(d.contains("-@preconcurrency import EventKit") && d.contains("+print(1)") && d.contains(' '));
        let ds = parse_diagnostics("/x/src/host/main.swift:3:5: error: cannot find 'foo' in scope\nnoise", Path::new("/x/src"), "m");
        assert_eq!(ds[0]["file"], "modules/m/host/main.swift");
        assert_eq!(ds[0]["line"], 3);
    }

    /// The app's Info.plist carries exactly the shared usage descriptions (and Bonjour list) the host plist embeds.
    #[test]
    fn usage_descriptions_match_app_plist() {
        let app = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Info.plist")).unwrap();
        let mut found = BTreeMap::new();
        let mut rest = app.as_str();
        while let Some(i) = rest.find("<key>") {
            rest = &rest[i + 5..];
            let (key, after) = rest.split_once("</key>").unwrap();
            if key.ends_with("UsageDescription") {
                let v = after.trim_start().strip_prefix("<string>").and_then(|x| x.split_once("</string>")).map(|(v, _)| v).unwrap_or("");
                found.insert(key.to_string(), v.to_string());
            }
            rest = after;
        }
        let want: BTreeMap<String, String> = USAGE_DESCRIPTIONS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(found, want, "src-tauri/Info.plist usage descriptions drifted from host_modules::USAGE_DESCRIPTIONS");
        let bonjour = app.split("<key>NSBonjourServices</key>").nth(1).and_then(|x| x.split("</array>").next()).unwrap_or("");
        let got: Vec<&str> = bonjour.split("<string>").skip(1).filter_map(|x| x.split("</string>").next()).collect();
        assert_eq!(got, BONJOUR_SERVICES);
        let hp = host_plist("ble-scan");
        assert!(hp.contains("<string>com.polygonface.openapp.host.ble-scan</string>") && hp.contains("NSBluetoothAlwaysUsageDescription"));
        assert!(USAGE_DESCRIPTIONS.iter().all(|(k, _)| hp.contains(&format!("<key>{k}</key>"))));
    }

    #[tokio::test]
    async fn host_log_rotates_at_cap() {
        let dir = std::env::temp_dir().join(format!("oa-logrot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("m.host.log");
        let (mut w, r) = tokio::io::duplex(1 << 16);
        let tail: crate::modules::Tail = Default::default();
        crate::modules::pump_stderr_capped(r, log.clone(), tail, Some(1000));
        use tokio::io::AsyncWriteExt;
        for i in 0..100 {
            w.write_all(format!("line {i:03} of the host module stderr\n").as_bytes()).await.unwrap();
        }
        drop(w);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let cur = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        let old = std::fs::read_to_string(dir.join("m.host.log.1")).unwrap_or_default();
        assert!(cur < 1000 && old.len() >= 1000 && old.len() < 1100, "rotated at the cap: current {cur} B, .1 {} B", old.len());
        assert!(std::fs::read_to_string(&log).unwrap().contains("line 099"), "latest lines in the current log");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
