//! Portable macOS bundle layout. A release build finds every helper it runs inside its own `.app`
//! (relative to `current_exe()`), never at a build-machine path. `pnpm tauri build` puts the pieces in
//! place through `tauri.conf.json` `bundle.macOS.files` (sources staged by `build.rs`):
//!
//! - `Contents/MacOS/msb` — microsandbox 0.7.8 runtime (keeps its own ad-hoc signature + hypervisor entitlement)
//! - `Contents/Frameworks/libkrunfw.5.dylib` — the matching libkrun firmware (path passed explicitly to msb)
//! - `Contents/MacOS/openapp-macos` — the Swift host-services sidecar (§18)
//! - `Contents/Helpers/OpenAppNotify.app` — the same sidecar with a bundle identity, for UserNotifications (§21 G4)
//!
//! Debug builds also accept the env overrides and the dev paths (`src-tauri/vm/runtime`, the cargo target dir).
//! A release build that misses a piece stops at startup with a clear message ([`check`]).

use std::path::{Path, PathBuf};

/// Paths relative to `Contents/`. Keep in sync with `tauri.conf.json` and `scripts/verify-bundle.ts`.
pub const MSB: &str = "MacOS/msb";
pub const LIBKRUNFW: &str = "Frameworks/libkrunfw.5.dylib";
pub const MACOS_SERVICES: &str = "MacOS/openapp-macos";
pub const NOTIFY_HELPER: &str = "Helpers/OpenAppNotify.app/Contents/MacOS/openapp-macos";

/// Every bundled piece: (label, path under Contents/, must be executable).
pub const PIECES: &[(&str, &str, bool)] = &[
    ("msb (VM runtime)", MSB, true),
    ("libkrunfw (VM firmware)", LIBKRUNFW, false),
    ("openapp-macos (host services)", MACOS_SERVICES, true),
    ("OpenAppNotify.app (notifications)", NOTIFY_HELPER, true),
];

/// `<App>.app/Contents` when this process runs from an app bundle.
pub fn contents_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    (contents.file_name()? == "Contents" && contents.join("Info.plist").is_file()).then(|| contents.to_path_buf())
}

/// A bundled piece by its path under `Contents/` (only when it exists).
pub fn piece(rel: &str) -> Option<PathBuf> {
    contents_dir().map(|c| c.join(rel)).filter(|p| p.exists())
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The error for a release build that runs without its bundled pieces.
pub fn missing(label: &str, rel: &str) -> String {
    match contents_dir() {
        Some(c) => format!(
            "OpenApp is incomplete: {label} is missing at {}. Reinstall OpenApp.",
            c.join(rel).display()
        ),
        None => format!("OpenApp must run from OpenApp.app ({label} is bundled at Contents/{rel})"),
    }
}

/// Release startup check: every piece is present (and executable where it runs).
pub fn check() -> Result<(), String> {
    let Some(c) = contents_dir() else {
        return Err("OpenApp must run from OpenApp.app: this binary is not inside an app bundle".into());
    };
    let mut bad = Vec::new();
    for (label, rel, exec) in PIECES {
        let p = c.join(rel);
        if !p.is_file() || (*exec && !is_exec(&p)) {
            bad.push(format!("{label}: Contents/{rel}"));
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(format!("OpenApp is incomplete. Missing: {}. Reinstall OpenApp.", bad.join("; "))) }
}

/// Show a fatal startup error (stderr + a native alert) and exit.
pub fn fatal(msg: &str) -> ! {
    eprintln!("OpenApp: {msg}");
    let esc = msg.replace('\\', "\\\\").replace('"', "\\\"");
    // the alert outlives this process (it gives up after 10 minutes)
    let _ = std::process::Command::new("osascript")
        .args(["-e", &format!("display alert \"OpenApp cannot start\" message \"{esc}\" as critical giving up after 600")])
        .spawn();
    std::process::exit(1);
}

/// The `openapp-macos` sidecar. Release: the bundled one only. Debug: `OPENAPP_MACOS_SERVICES`, the
/// bundle, then the binary build.rs built in the cargo target dir.
pub fn macos_services() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        if let Some(p) = std::env::var_os("OPENAPP_MACOS_SERVICES").filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(p));
        }
        if let Some(p) = piece(MACOS_SERVICES) {
            return Some(p);
        }
        option_env!("OPENAPP_MACOS_SERVICES_BIN").map(PathBuf::from).filter(|p| p.is_file())
    }
    #[cfg(not(debug_assertions))]
    {
        piece(MACOS_SERVICES)
    }
}

/// The notification helper (`openapp-macos` inside `OpenAppNotify.app`). Same order as [`macos_services`]
/// with `OPENAPP_NOTIFY_HELPER`.
pub fn notify_helper() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        if let Some(p) = std::env::var_os("OPENAPP_NOTIFY_HELPER").filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(p));
        }
        if let Some(p) = piece(NOTIFY_HELPER) {
            return Some(p);
        }
        option_env!("OPENAPP_NOTIFY_HELPER_BIN").map(PathBuf::from).filter(|p| p.is_file())
    }
    #[cfg(not(debug_assertions))]
    {
        piece(NOTIFY_HELPER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_relative_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for (_, rel, _) in PIECES {
            assert!(!rel.starts_with('/') && !rel.contains(".."), "{rel}");
            assert!(seen.insert(*rel));
        }
        // a test binary is not inside an app bundle
        assert!(contents_dir().is_none());
        assert!(check().is_err());
    }
}
