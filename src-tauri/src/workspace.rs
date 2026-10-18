//! Workspace init, PATH discovery and the 500 ms module watcher (contract §2, §3).

use crate::kernel::Kernel;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const BUILDER_GUIDE: &str = include_str!("../../workspace-template/AGENTS.md");
pub const BUILDER_SKILL: &str = include_str!("../../workspace-template/skills/openapp-builder/SKILL.md");
pub const SKILL_NAME: &str = "openapp-builder";
/// The skill's on-demand reference files (`reference/<name>`), listed in the core SKILL.md.
pub const BUILDER_REFERENCE: &[(&str, &str)] = &[
    ("host-modules.md", include_str!("../../workspace-template/skills/openapp-builder/reference/host-modules.md")),
    ("connect-recipes.md", include_str!("../../workspace-template/skills/openapp-builder/reference/connect-recipes.md")),
    ("services-ports-pty-display.md", include_str!("../../workspace-template/skills/openapp-builder/reference/services-ports-pty-display.md")),
    ("native-shell-and-files.md", include_str!("../../workspace-template/skills/openapp-builder/reference/native-shell-and-files.md")),
    ("jobs-agents-secret-release.md", include_str!("../../workspace-template/skills/openapp-builder/reference/jobs-agents-secret-release.md")),
    ("channels.md", include_str!("../../workspace-template/skills/openapp-builder/reference/channels.md")),
    ("host-data.md", include_str!("../../workspace-template/skills/openapp-builder/reference/host-data.md")),
];

/// The placement rules: the bullets between the `placement-rules` markers of the skill (one source for the skill and
/// `openapp_describe` → `placement_rules`).
pub fn placement_rules() -> Vec<&'static str> {
    let Some((_, rest)) = BUILDER_SKILL.split_once("<!-- placement-rules -->") else { return Vec::new() };
    let section = rest.split_once("<!-- /placement-rules -->").map_or(rest, |(s, _)| s);
    section.lines().filter_map(|l| l.trim().strip_prefix("- ")).collect()
}

/// `<workspace>/.openapp/skills` (Codex extra skills root).
pub fn skills_root(ws: &Path) -> PathBuf {
    ws.join(".openapp").join("skills")
}

/// `<workspace>/.openapp/skills/openapp-builder/SKILL.md`
pub fn builder_skill_path(ws: &Path) -> PathBuf {
    skills_root(ws).join(SKILL_NAME).join("SKILL.md")
}

fn write_if_differs(p: &Path, content: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(p).ok().as_deref() != Some(content) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, content)?;
    }
    Ok(())
}
const WORKSPACE_AGENT_SETTINGS: &str = r#"{
  "permissions": {
    "defaultMode": "acceptEdits",
    "allow": ["mcp__openapp"]
  }
}
"#;

/// `OPENAPP_WORKSPACE` or `~/.openapp/workspace`.
pub fn resolve_workspace() -> PathBuf {
    if let Some(p) = std::env::var_os("OPENAPP_WORKSPACE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".openapp").join("workspace")
}


/// Init: create dirs, `git init` (ignore failure), write/refresh AGENTS.md + CLAUDE.md.
pub fn init(ws: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(ws.join("modules"))?;
    std::fs::create_dir_all(ws.join(".openapp").join("logs"))?;
    if !ws.join(".git").exists() {
        let _ = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(ws)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // The kernel owns the builder guide: refresh it whenever it differs from the embedded one.
    for name in ["AGENTS.md", "CLAUDE.md"] {
        write_if_differs(&ws.join(name), BUILDER_GUIDE)?;
    }
    // Kernel-owned skills (§12.2) with their reference files; mirrored for Claude Code.
    for dir in [skills_root(ws).join(SKILL_NAME), ws.join(".claude").join("skills").join(SKILL_NAME)] {
        write_if_differs(&dir.join("SKILL.md"), BUILDER_SKILL)?;
        for (name, text) in BUILDER_REFERENCE {
            write_if_differs(&dir.join("reference").join(name), text)?;
        }
    }
    // Construction tools and edits inside the workspace are the agent's normal job; other
    // actions (shell commands, network) still reach the user as permission cards.
    let settings = ws.join(".claude").join("settings.json");
    if !settings.exists() {
        std::fs::create_dir_all(ws.join(".claude"))?;
        std::fs::write(&settings, WORKSPACE_AGENT_SETTINGS)?;
    }
    Ok(())
}

/// Module folder names currently on disk (those with an openapp.json or any folder).
pub fn module_dirs(ws: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(ws.join("modules")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) && e.path().join("openapp.json").exists() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// PATH for child processes. GUI apps on macOS start with a minimal PATH, so ask
/// the login shell for the user's PATH and merge it with common tool dirs.
pub fn login_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    #[cfg(unix)]
    if let Some(p) = shell_path() {
        parts.extend(std::env::split_paths(&p).map(|p| p.to_string_lossy().to_string()));
    }
    parts.extend(std::env::split_paths(&current).map(|p| p.to_string_lossy().to_string()));
    let home = dirs::home_dir().unwrap_or_default();
    for extra in [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
        home.join(".cargo/bin"),
        home.join(".bun/bin"),
        home.join(".deno/bin"),
        home.join(".local/bin"),
    ] {
        if extra.is_dir() {
            parts.push(extra.to_string_lossy().to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    parts.retain(|p| !p.is_empty() && seen.insert(p.clone()));
    std::env::join_paths(parts).map(|s| s.to_string_lossy().to_string()).unwrap_or(current)
}

#[cfg(unix)]
fn shell_path() -> Option<String> {
    use std::io::Read;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = std::process::Command::new(shell)
        .args(["-ilc", "printf '__OAPATH__%s__OAPATH__' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(4) => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut s = String::new();
    child.stdout.take()?.read_to_string(&mut s).ok()?;
    let a = s.find("__OAPATH__")? + "__OAPATH__".len();
    let b = s[a..].find("__OAPATH__")? + a;
    Some(s[a..b].to_string())
}

/// Poll `modules/` every 500 ms; push view changes live.
pub async fn watch(k: Arc<Kernel>) {
    let mut iv = tokio::time::interval(Duration::from_millis(500));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        iv.tick().await;
        let k2 = k.clone();
        // file IO off the async workers
        let changed = tokio::task::spawn_blocking(move || k2.poll_changes()).await.unwrap_or(false);
        if changed {
            k.emit_snapshot();
        }
        if k.is_shutdown() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_core_lists_reference_and_rules() {
        assert!(BUILDER_SKILL.lines().count() <= 160, "core SKILL.md stays short");
        for (name, text) in BUILDER_REFERENCE {
            assert!(BUILDER_SKILL.contains(&format!("`{name}`")), "SKILL.md lists reference/{name}");
            assert!(text.starts_with("# "), "{name} has a title");
        }
        let rules = placement_rules();
        assert!(rules.len() >= 4 && rules.iter().any(|r| r.contains("view.open")), "{rules:?}");
        // loop-3 rules stay in the core: live data, non-HTTP via SOCKS, host-module triggers, secret host binding
        for needle in ["users never reload", "$stream", "$OPENAPP_SOCKS_URL", "\"net\": \"socks\"", "net.imap", "Bluetooth, Calendar, Contacts", "reference/host-modules.md", "never `\"*\"`", "type=module"] {
            assert!(BUILDER_SKILL.contains(needle), "SKILL.md keeps {needle:?}");
        }
        let connect = BUILDER_REFERENCE.iter().find(|(n, _)| *n == "connect-recipes.md").map(|(_, t)| *t).unwrap_or_default();
        assert!(connect.contains("socks5h://") && connect.contains("OPENAPP_SOCKS_URL") && !BUILDER_SKILL.contains("$ALL_PROXY"), "connect-recipes.md documents SOCKS");
    }
}
