//! Path mapping between what callers name (`/workspace/...` in the VM, workspace-relative, absolute host paths inside
//! the workspace, `/mnt/<grant>/...` for granted folders) and host paths. Everything else is refused.

use crate::kernel::Kernel;
use std::path::{Component, Path, PathBuf};

/// Map a caller's path (`/workspace/...`, workspace-relative, or an absolute host path inside the
/// workspace) to a host path inside `ws`. `must_exist`: input file (symlinks resolved and checked).
pub fn map_path(ws: &Path, p: &str, must_exist: bool) -> Result<PathBuf, String> {
    map_in(ws, &crate::state::state_dir(ws), p, must_exist)
}

fn map_in(ws: &Path, state_dir: &Path, p: &str, must_exist: bool) -> Result<PathBuf, String> {
    // §21 G5: `/mnt/<grant>/…` = a folder the user granted (mounted at the same path in the VM)
    if let Some(r) = map_mnt(state_dir, p, must_exist) {
        return r;
    }
    let rel: PathBuf = if let Some(r) = p.strip_prefix(crate::vm::GUEST_WS) {
        if !(r.is_empty() || r.starts_with('/')) {
            return Err(format!("path {p} is outside the workspace"));
        }
        PathBuf::from(r.trim_start_matches('/'))
    } else if Path::new(p).is_absolute() {
        Path::new(p).strip_prefix(ws).map(Path::to_path_buf).map_err(|_| format!("path {p} is outside the workspace"))?
    } else {
        PathBuf::from(p)
    };
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("path {p} must not contain \"..\""));
    }
    let host = ws.join(&rel);
    let check = if must_exist {
        host.canonicalize().map_err(|_| format!("file not found: {p}"))?
    } else {
        let parent = host.parent().unwrap_or(ws);
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        parent.canonicalize().map_err(|e| e.to_string())?.join(host.file_name().unwrap_or_default())
    };
    let wsc = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
    if !check.starts_with(&wsc) {
        return Err(format!("path {p} is outside the workspace"));
    }
    Ok(check)
}

/// `/mnt/<name>/…` → host path inside that grant (no `..`; symlinks may not leave it).
fn map_mnt(state_dir: &Path, p: &str, must_exist: bool) -> Option<Result<PathBuf, String>> {
    let rest = p.strip_prefix("/mnt/")?;
    let (name, rel) = rest.split_once('/').unwrap_or((rest, ""));
    let f = crate::primitives::files::resolved(state_dir).into_iter().find(|f| f["name"] == name);
    let Some(f) = f else { return Some(Err(format!("no granted folder at /mnt/{name}"))) };
    let root = PathBuf::from(f["path"].as_str().unwrap_or(""));
    if Path::new(rel).components().any(|c| !matches!(c, Component::Normal(_))) {
        return Some(Err(format!("path {p} must not contain \"..\"")));
    }
    let host = root.join(rel);
    let check = if must_exist {
        match host.canonicalize() {
            Ok(c) => c,
            Err(_) => return Some(Err(format!("file not found: {p}"))),
        }
    } else {
        let parent = host.parent().unwrap_or(&root).to_path_buf();
        if let Err(e) = std::fs::create_dir_all(&parent) {
            return Some(Err(e.to_string()));
        }
        parent.canonicalize().unwrap_or(parent).join(host.file_name().unwrap_or_default())
    };
    let rootc = root.canonicalize().unwrap_or(root);
    Some(if check.starts_with(&rootc) { Ok(check) } else { Err(format!("path {p} leaves the granted folder")) })
}

/// Any path a module may name: `/mnt/<grant>/…`, `/workspace/…`, workspace-relative or inside the workspace.
pub fn map_any(k: &Kernel, p: &str, must_exist: bool) -> Result<PathBuf, String> {
    map_in(&k.workspace, &k.state_dir, p, must_exist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let d = std::env::temp_dir().join("oa-host-paths");
        let _ = std::fs::create_dir_all(d.join("modules/m"));
        std::fs::write(d.join("modules/m/a.txt"), "x").unwrap();
        let ws = d.canonicalize().unwrap();
        assert_eq!(map_path(&ws, "/workspace/modules/m/a.txt", true).unwrap(), ws.join("modules/m/a.txt"));
        assert_eq!(map_path(&ws, "modules/m/a.txt", true).unwrap(), ws.join("modules/m/a.txt"));
        assert!(map_path(&ws, &ws.join("modules/m/a.txt").to_string_lossy(), true).is_ok());
        assert!(map_path(&ws, "/etc/passwd", true).is_err());
        assert!(map_path(&ws, "/workspace/../etc/passwd", true).is_err());
        assert!(map_path(&ws, "/workspacex/a", false).is_err());
        assert!(map_path(&ws, "out/new.m4a", false).is_ok());
    }
}
