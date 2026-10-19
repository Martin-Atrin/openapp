//! Agent harnesses (contract §11, §17): detection, the provider picker, and dispatch to the
//! native Codex app-server adapter, the Pi RPC adapter or the ACP adapter. `codex` and `pi` run
//! inside the app VM (bypass mode, own login). `OPENAPP_HARNESS=<id>` is an optional override for smoke tests; normally the user picks and the choice persists in `<state>/harness.json`.

use crate::research::Kind;
use crate::util::MutexExt;
use crate::kernel::{Kernel, PermWaiter};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Adapter {
    #[serde(rename = "codex-app-server")]
    Codex,
    #[serde(rename = "acp")]
    Acp,
    /// `pi --mode rpc` (JSONL commands/events), in the app VM.
    #[serde(rename = "pi-rpc")]
    Pi,
}

/// Release gating (contract §11): the only provider ids a release build can use. Every other
/// provider stays in the list as "Coming soon" (never probed, never selectable).
pub const RELEASE_PROVIDERS: &[&str] = &["pi"];

/// Gating is on in release builds, and in debug builds with `OPENAPP_RELEASE_GATING=1`.
pub fn release_gated() -> bool {
    !cfg!(debug_assertions) || std::env::var("OPENAPP_RELEASE_GATING").is_ok_and(|v| v.trim() == "1")
}

/// May `id` be probed, selected, loaded or started under the given gating?
pub fn allowed_with(gated: bool, id: &str) -> bool {
    !gated || RELEASE_PROVIDERS.contains(&id)
}

pub fn is_allowed(id: &str) -> bool {
    allowed_with(release_gated(), id)
}

/// Error for a provider id that gating refuses (or that does not exist).
fn refused(id: &str) -> String {
    if specs_with(None, false).iter().any(|s| s.id == id) {
        format!("provider \"{id}\" is coming soon")
    } else {
        format!("unknown provider \"{id}\"")
    }
}

/// A launchable harness (contract §11).
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessSpec {
    pub id: String,
    pub name: String,
    pub adapter: Adapter,
    /// ACP launch command (run with `sh -c`); unused for Codex.
    pub cmd: String,
    /// Runs inside the app VM (§17).
    pub vm: bool,
}

/// `HarnessInfo` in the snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HarnessInfo {
    pub id: String,
    pub name: String,
    pub adapter: Adapter,
    pub available: bool,
    #[serde(rename = "signedIn")]
    pub signed_in: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Runs inside the app VM (§17).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub vm: bool,
    /// Sign-in flow state for VM harnesses: `{ state: "required"|"starting"|"browser"|"device"|"failed",
    /// url?, code?, message? }` (absent when signed in or unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<serde_json::Value>,
    /// VM harnesses (Pi): `{ logins: [{ provider, type, label, active }], methods }` for the account card: sign in
    /// again, add a provider, replace an API key, sign out. Never holds a credential value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts: Option<serde_json::Value>,
    /// Release gating: not usable in this build ("Coming soon"); the card is dimmed and inert.
    #[serde(rename = "comingSoon", skip_serializing_if = "std::ops::Not::not")]
    pub coming_soon: bool,
}

/// Live state the kernel learned about a harness after it started (sign-in, login flow);
/// laid over the detection result.
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub signed_in: Option<bool>,
    pub note: Option<String>,
    pub login: Option<serde_json::Value>,
    pub accounts: Option<serde_json::Value>,
    /// §23 the current model (`GPT-X · High`), appended to the card note.
    pub model_note: Option<String>,
}

fn spec(id: &str, name: &str, adapter: Adapter, cmd: &str) -> HarnessSpec {
    HarnessSpec { id: id.into(), name: name.into(), adapter, cmd: cmd.into(), vm: false }
}

/// Built-in harnesses plus `custom` (from `OPENAPP_AGENT_CMD`; never in gated mode).
pub fn specs(custom: Option<&str>) -> Vec<HarnessSpec> {
    specs_with(custom, release_gated())
}

pub fn specs_with(custom: Option<&str>, gated: bool) -> Vec<HarnessSpec> {
    let mut v = vec![
        HarnessSpec { vm: true, ..spec("codex", "Codex · isolated VM", Adapter::Codex, "") },
        HarnessSpec { vm: true, ..spec("pi", "Pi · isolated VM", Adapter::Pi, "") },
        spec("claude", "Claude Code", Adapter::Acp, "npx -y @agentclientprotocol/claude-agent-acp"),
        spec("gemini", "Gemini CLI", Adapter::Acp, "gemini --acp"),
        spec("goose", "Goose", Adapter::Acp, "goose acp"),
        spec("opencode", "OpenCode", Adapter::Acp, "opencode acp"),
    ];
    if let Some(cmd) = custom.filter(|s| !s.trim().is_empty() && allowed_with(gated, "custom")) {
        v.push(custom_spec(cmd));
    }
    v
}

pub fn custom_spec(cmd: &str) -> HarnessSpec {
    let short: String = cmd.chars().take(40).collect();
    spec("custom", &format!("Custom ({short})"), Adapter::Acp, cmd)
}

/// The spec for `id`; `None` when it does not exist or gating refuses it.
pub fn spec_by_id(custom: Option<&str>, id: &str) -> Option<HarnessSpec> {
    if !is_allowed(id) {
        return None;
    }
    specs(custom).into_iter().find(|s| s.id == id)
}

/// `OPENAPP_AGENT_CMD`, ignored in gated mode.
pub fn custom_from_env() -> Option<String> {
    if !is_allowed("custom") {
        return None;
    }
    std::env::var("OPENAPP_AGENT_CMD").ok().filter(|s| !s.trim().is_empty())
}

/// Optional override for smoke tests: `OPENAPP_HARNESS=<id>` (`acp` = custom/claude).
pub fn env_override(custom: Option<&str>) -> Option<HarnessSpec> {
    let id = std::env::var("OPENAPP_HARNESS").ok()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    if id == "acp" {
        return spec_by_id(custom, "custom").or_else(|| spec_by_id(custom, "claude"));
    }
    // spec_by_id refuses gated ids, so a stray OPENAPP_HARNESS cannot bypass gating
    spec_by_id(custom, &id)
}

async fn detect_one(path_env: String, s: HarnessSpec, gated: bool) -> HarnessInfo {
    let mut info = HarnessInfo {
        id: s.id.clone(),
        name: s.name.clone(),
        adapter: s.adapter,
        available: false,
        signed_in: None,
        note: None,
        vm: s.vm,
        login: None,
        accounts: None,
        coming_soon: false,
    };
    // Gated: answer without touching the system (no PATH lookup, no probe, no ~/.codex or ~/.gemini read).
    if !allowed_with(gated, &s.id) {
        info.coming_soon = true;
        info.note = Some("Coming soon".into());
        return info;
    }
    if s.vm {
        match crate::vm::env_check() {
            Ok(_) => {
                info.available = true;
                info.note = Some("Runs in an isolated Linux VM with its own sign-in".into());
            }
            Err(e) => info.note = Some(e),
        }
        return info;
    }
    let bin = match s.id.as_str() {
        "custom" => s.cmd.split_whitespace().next().unwrap_or("").to_string(),
        other => other.to_string(),
    };
    if crate::modules::which(&bin, &path_env).is_none() {
        info.note = Some(if s.id == "custom" { format!("`{bin}` not found on PATH") } else { "Not installed".into() });
        return info;
    }
    info.available = true;
    match s.id.as_str() {
        "claude" => {
            // `claude auth status` can report loggedIn while the stored token is stale, so the
            // real state is only known when the harness starts.
            info.note = Some("Installed; sign-in is checked when it starts".into());
            if crate::modules::which("npx", &path_env).is_none() {
                info.available = false;
                info.note = Some("Needs `npx` (Node.js) for the ACP adapter".into());
            }
        }
        "gemini" => {
            let has_key = ["GEMINI_API_KEY", "GOOGLE_API_KEY"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
            let has_login = dirs::home_dir().is_some_and(|h| h.join(".gemini").join("oauth_creds.json").exists());
            if has_key {
                info.signed_in = Some(true);
                info.note = Some("API key set".into());
            } else if has_login {
                info.note = Some("Google login found".into());
            } else {
                info.signed_in = Some(false);
                info.note = Some("No API key or Google login found: run `gemini` once to sign in".into());
            }
        }
        "custom" => info.note = Some(s.cmd.clone()),
        _ => info.note = Some("Installed; sign-in is checked when it starts".into()),
    }
    info
}

/// Detect all harnesses in parallel (3 s timeout per check).
pub async fn detect(path_env: &str, custom: Option<&str>) -> Vec<HarnessInfo> {
    detect_with(path_env, custom, release_gated()).await
}

pub async fn detect_with(path_env: &str, custom: Option<&str>, gated: bool) -> Vec<HarnessInfo> {
    let tasks: Vec<_> =
        specs_with(custom, gated).into_iter().map(|s| tokio::spawn(detect_one(path_env.to_string(), s, gated))).collect();
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(i) = t.await {
            out.push(i);
        }
    }
    out
}

pub fn harness_file(k: &Kernel) -> std::path::PathBuf {
    k.state_dir.join("harness.json")
}

pub fn load_saved(k: &Kernel) -> Option<HarnessSpec> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(harness_file(k)).ok()?).ok()?;
    let id = v.get("id")?.as_str()?;
    let spec = spec_by_id(k.custom_cmd.as_deref(), id);
    if spec.is_none() && specs_with(None, false).iter().any(|s| s.id == id) && !is_allowed(id) {
        // stale choice from a build without gating: forget it and start at the picker
        save_choice(k, None);
        k.system_message("system", &format!("Your saved provider ({id}) is coming soon. Pick a provider to continue."));
    }
    spec
}

fn save_choice(k: &Kernel, id: Option<&str>) {
    let p = harness_file(k);
    match id {
        Some(id) => {
            let _ = std::fs::write(p, serde_json::json!({ "id": id }).to_string());
        }
        None => {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub fn selected(k: &Kernel) -> Option<HarnessSpec> {
    k.harness.locked().clone()
}

/// Stop the running harness process (no error is reported for it).
pub fn stop(k: &Kernel) {
    let acp = k.acp.locked().take();
    let codex = k.codex.locked().take();
    let pi = k.pi.locked().take();
    if let Some(pid) = acp.as_ref().and_then(|c| c.pid) {
        kill_group(pid);
    }
    if let Some(c) = codex.as_ref() {
        c.terminate();
    }
    if let Some(c) = pi.as_ref() {
        c.terminate();
    }
    cancel_open_permissions(k, None);
    k.working.store(0, Ordering::Relaxed);
}

/// Pick a harness: stop the old one, persist the choice, start the new one.
pub fn select(k: &Arc<Kernel>, id: &str) -> Result<(), String> {
    let spec = spec_by_id(k.custom_cmd.as_deref(), id).ok_or_else(|| {
        format!(
            "{} (providers: {})",
            refused(id),
            specs(k.custom_cmd.as_deref()).iter().filter(|s| is_allowed(&s.id)).map(|s| s.id.clone()).collect::<Vec<_>>().join(", ")
        )
    })?;
    if let Some(info) = k.harnesses.locked().iter().find(|h| h.id == id)
        && !info.available {
            return Err(format!("{} is not available: {}", info.name, info.note.clone().unwrap_or_default()));
        }
    let prev = selected(k);
    stop(k);
    save_choice(k, Some(id));
    *k.harness.locked() = Some(spec.clone());
    {
        let mut g = k.lock();
        g.agent.name = spec.name.clone();
        g.agent.harness = Some(spec.id.clone());
    }
    k.set_agent("starting", None);
    let msg = match prev {
        Some(p) if p.id != spec.id => format!("Switched provider from {} to {}.", p.name, spec.name),
        Some(_) => format!("Restarted {}.", spec.name),
        None => format!("Provider: {}.", spec.name),
    };
    k.system_message("system", &msg);
    k.research.log("user", Kind::HarnessSelect, serde_json::json!({ "id": spec.id }));
    tokio::spawn(run(k.clone()));
    Ok(())
}

/// Restart the selected harness (e.g. after a sign-in stored a new credential).
pub fn restart(k: &Arc<Kernel>) {
    stop(k);
    k.set_agent("starting", None);
    tokio::spawn(run(k.clone()));
}

/// Host action / command `harness.login {id, method?}`: start the sign-in flow of a VM harness
/// (browser login with the callback forwarded into the VM). Codex: `method: "device"` = device
/// code. Pi: `method` = `chatgpt` (default) | `claude`; `cancel` closes an open flow.
pub fn login(k: &Arc<Kernel>, id: &str, method: Option<&str>) -> Result<(), String> {
    let spec = spec_by_id(k.custom_cmd.as_deref(), id).ok_or_else(|| refused(id))?;
    if !spec.vm {
        return Err(format!("{} signs in outside OpenApp (run its own login command in a terminal)", spec.name));
    }
    if selected(k).map(|s| s.id) != Some(spec.id.clone()) {
        select(k, id)?;
    }
    if spec.adapter == Adapter::Pi {
        if method == Some("cancel") {
            crate::pi::login_cancel(k);
            return Ok(());
        }
        let method = method.unwrap_or("chatgpt").to_string();
        let k = k.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(e) = crate::pi::login(&k, &method, true).await {
                k.update_harness(&id, |o| {
                    o.login = Some(serde_json::json!({ "state": "failed", "message": e, "methods": crate::pi::methods() }));
                    o.note = Some(format!("Sign-in failed: {e}"));
                });
            }
        });
        return Ok(());
    }
    let device = method == Some("device");
    let k = k.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        if let Err(e) = crate::codex::login(&k, device, true).await {
            k.update_harness(&id, |o| {
                o.login = Some(serde_json::json!({ "state": "failed", "message": e }));
                o.note = Some(format!("Sign-in failed: {e}"));
            });
        }
    });
    Ok(())
}

/// Command `harness_login_submit {id, kind, value, provider?}`: the user's input for an open sign-in
/// (Pi: `code` = pasted redirect URL, `apikey` = provider API key). The value is never logged.
pub async fn login_submit(k: &Arc<Kernel>, id: &str, kind: &str, value: &str, provider: Option<&str>) -> Result<(), String> {
    let spec = spec_by_id(k.custom_cmd.as_deref(), id).ok_or_else(|| refused(id))?;
    match spec.adapter {
        Adapter::Pi => crate::pi::login_submit(k, kind, value, provider).await,
        _ => Err(format!("{} does not take sign-in input here", spec.name)),
    }
}

/// Command `harness_logout {id, provider}`: remove one stored login of a VM harness (Pi) and restart it.
pub async fn logout(k: &Arc<Kernel>, id: &str, provider: &str) -> Result<(), String> {
    let spec = spec_by_id(k.custom_cmd.as_deref(), id).ok_or_else(|| refused(id))?;
    match spec.adapter {
        Adapter::Pi => crate::pi::logout(k, provider).await,
        _ => Err(format!("{} keeps its login outside OpenApp", spec.name)),
    }
}

/// `/provider`: stop the harness and go back to the picker.
pub fn unselect(k: &Kernel) {
    stop(k);
    save_choice(k, None);
    *k.harness.locked() = None;
    {
        let mut g = k.lock();
        g.agent.harness = None;
        g.agent.name = "No provider".into();
    }
    k.set_agent("unselected", None);
}

/// Re-run detection and push a snapshot.
pub async fn refresh(k: &Arc<Kernel>) {
    let list = detect(k.path_env(), k.custom_cmd.as_deref()).await;
    k.set_harnesses(list);
    k.emit_snapshot();
}

/// Start the selected harness process and handshake.
pub async fn run(k: Arc<Kernel>) {
    match selected(&k) {
        Some(s) if s.adapter == Adapter::Codex => crate::codex::run(k, s).await,
        Some(s) if s.adapter == Adapter::Pi => crate::pi::run(k, s).await,
        Some(s) => crate::acp::run(k, s).await,
        None => k.set_agent("unselected", None),
    }
}

/// One user turn in an OpenApp session.
pub async fn run_turn(k: Arc<Kernel>, session: String, text: String, build: bool) {
    match selected(&k).map(|s| s.adapter) {
        Some(Adapter::Codex) => crate::codex::run_turn(k, session, text, build).await,
        Some(Adapter::Pi) => crate::pi::run_turn(k, session, text, build).await,
        Some(Adapter::Acp) => crate::acp::run_turn(k, session, text, build).await,
        None => {
            let _tg = crate::kernel::TurnGuard::begin(&k, &session);
            k.system_message_in(&session, "error", "No provider is picked yet. Pick one to start (or type /provider).");
        }
    }
}

/// Cancel the active session's turn.
pub fn cancel(k: &Kernel) {
    crate::codex::cancel(k);
    crate::pi::cancel(k);
    crate::acp::cancel(k);
    cancel_open_permissions(k, Some(&k.active_session()));
}

/// Forget the harness-side conversation of a session (after /reset).
pub fn forget_session(k: &Kernel, session: &str) {
    crate::acp::forget_session(k, session);
    crate::codex::forget_session(k, session);
    crate::pi::forget_session(k, session);
}

pub fn kill(k: &Kernel) {
    crate::acp::kill(k);
    crate::codex::kill(k);
    crate::pi::kill(k);
}

#[cfg(unix)]
pub fn kill_group(pid: u32) {
    let _ = std::process::Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).status();
}
#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {}

/// Containment rules for every agent thread (§12.3).
pub const CONTAINMENT: &str = "You are the builder inside OpenApp. The app VM and this workspace are your own world: you have a full Linux toolchain, network through OpenApp (the user approves new hosts), the openapp tools, the Mac's services through the bridge, and the OpenApp skills. Nothing the user asks for is pre-installed: features, connectors, agents, schedulers and screens do not exist until you build them. When something is missing, that is your task, not a limit: design it, ask the user only for decisions and credentials (question tool), build it, test it, and show it. Never answer that you lack a feature, connector or integration. Only a true kernel gap (openapp_describe -> kernel_gaps) is reported, and then with the closest working alternative you can build now. Stay inside your world: do not inspect files, processes, configs or memories outside the workspace and the VM.";

/// Extra rule for agents in the app VM (§17).
pub const VM_RULE: &str = "You run inside OpenApp's Linux VM (Debian stable: node, python3, git, ffmpeg, build-essential; apt-get works through the proxy). /workspace is the user's app and is shared with the user's computer; edits made outside the VM can take about 5 seconds to appear in /workspace. You have no direct network: HTTP(S) goes through OpenApp's proxy (already configured in the environment), which allows package registries and your provider, and asks the user for any other host; if a host is denied, say so and do not retry in a loop.";

/// Chat/build mode rule for every agent thread (§12.1).
/// §19.2 build mode: the user sees only questions and results.
pub const BUILD_RULE: &str = "Build mode rules: ask the user questions only through the question tool, never in plain text; otherwise build without narration (no progress messages, no plans in prose); end with one or two sentences saying what changed and where it is in the app. Build the smallest thing that works: if one host action does it, write one spec view and no server. Place views by the placement rules (the skill; openapp_describe → placement_rules). Use OpenApp host services first (openapp_describe → host_services). Never ask for tokens, passwords or keys in chat or in question cards: use openapp_secret_request, then net.fetch / net.imap with auth.secret.";

pub const MODE_RULE: &str = "The user's messages arrive in chat mode unless marked as build mode. In chat mode answer normally and do not change the app; if the user asks for something that needs building, say you will build it and ask them to send it with /build (one line, e.g. \"I can build that: send /build connect my Gmail\"). In build mode follow the openapp-builder skill and the build mode rules.";

/// Prefix for ACP build-mode turns (§12.1): ACP has no skill input item, so the agent reads the skill itself.
pub fn acp_build_prefix(k: &Kernel) -> String {
    let skill = crate::workspace::builder_skill_path(&k.workspace);
    format!("OpenApp build mode. Follow the skill at {} (read it first). {BUILD_RULE}", skill.display())
}

/// Env vars of a Claude Code host session that must not leak into a nested harness.
pub const INHERITED_HOST_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_PID",
];

/// Drop the env of a Claude Code host session (dev runs started from one): a nested Claude would try to reuse the
/// parent's auth channel and fail.
pub fn sanitize_env(cmd: &mut tokio::process::Command) {
    for var in INHERITED_HOST_SESSION_VARS {
        cmd.env_remove(var);
    }
}

/// `sh -c <cmd>` in the workspace with the login PATH and a sanitized env, piped stdio, its own process group.
pub fn host_command(k: &Kernel, cmd: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(cmd).current_dir(&k.workspace).env("PATH", k.path_env());
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    sanitize_env(&mut c);
    #[cfg(unix)]
    c.process_group(0);
    c
}

/// `<workspace>/.openapp/logs/_agent.log` with a start line for `what`.
pub fn agent_log(k: &Kernel, what: &str) -> std::path::PathBuf {
    let log_path = k.workspace.join(".openapp").join("logs").join("_agent.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = writeln!(f, "=== {} start: {what} ===", crate::state::now_ts());
    }
    log_path
}

/// A pinned npm package for the VM volume (see [`install_in_vm`]).
pub struct NpmInstall<'a> {
    /// label for the card note, the log and the research entry, e.g. "Pi"
    pub name: &'a str,
    pub version: &'a str,
    /// `package@version`
    pub npm_spec: &'a str,
    /// install prefix (on /opt/openapp)
    pub dir: &'a str,
    /// shell test: already installed
    pub check: &'a str,
    /// shell command after the install, e.g. `<bin> --version`
    pub verify: &'a str,
}

/// Install a pinned npm package into the VM volume (once; cached on /opt/openapp).
pub async fn install_in_vm(k: &Kernel, vm: &crate::vm::AppVm, harness_id: &str, p: &NpmInstall<'_>) -> Result<(), String> {
    let NpmInstall { name, version, npm_spec, dir, check, verify } = p;
    let (code, _, _) = vm.sh(check, &[]).await?;
    if code == 0 {
        return Ok(());
    }
    k.update_harness(harness_id, |o| o.note = Some(format!("Installing {name} {version} in the app VM…")));
    let t = std::time::Instant::now();
    let script = format!("set -e; rm -rf {dir}.tmp; mkdir -p {dir}.tmp; npm i -g --no-fund --no-audit --loglevel=error --prefix {dir}.tmp {npm_spec}; rm -rf {dir}; mv {dir}.tmp {dir}; {verify}");
    let (code, out, err) = vm.sh(&script, &crate::vm::proxy_env("kernel")).await?;
    let ms = t.elapsed().as_millis() as u64;
    k.research.log("kernel", Kind::HarnessInstall, serde_json::json!({ "harness": harness_id, "version": version, "ok": code == 0, "ms": ms }));
    if code != 0 {
        let last: Vec<&str> = err.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(format!("installing {name} {version} in the app VM failed (exit {code}):\n{}", last.join("\n")));
    }
    log::info!("{name} installed in the app VM in {ms} ms: {}", out.trim());
    Ok(())
}

/// The harness process exited. Unless OpenApp quits or a newer harness replaced it (`current` false): agent error,
/// chat error with the last stderr lines, research `turn_end` with `harness_exit`.
pub fn report_exit(k: &Kernel, current: bool, what: &str, status: &str, tail: &crate::modules::Tail) {
    if k.is_shutdown() || !current {
        return;
    }
    let last = crate::modules::tail_lines(tail, 15).join("\n");
    let msg = format!("{what} exited ({status}). Last stderr:\n{last}");
    k.working.store(0, Ordering::Relaxed);
    k.set_agent("error", Some(msg.clone()));
    k.system_message("error", &msg);
    k.research.log("kernel", Kind::TurnEnd, serde_json::json!({ "harness_exit": status }));
}

pub fn cancel_open_permissions(k: &Kernel, session: Option<&str>) {
    let open: Vec<PermWaiter> = {
        let mut perms = k.perms.locked();
        let ids: Vec<String> = perms
            .iter()
            .filter(|(_, w)| match session {
                None => true,
                // kernel capability cards outlive agent turns
                Some(s) => w.session == s && !w.kernel_owned,
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter().filter_map(|id| perms.remove(&id)).collect()
    };
    for w in open {
        let _ = w.tx.send(None);
    }
    let qs: Vec<String> = k
        .questions
        .locked()
        .iter()
        .filter(|(_, w)| session.is_none_or(|s| w.session == s))
        .map(|(id, _)| id.clone())
        .collect();
    for id in qs {
        let _ = k.answer_question(&id, None);
    }
}

pub fn turn_started(k: &Kernel) {
    if k.working.fetch_add(1, Ordering::Relaxed) == 0 {
        k.set_agent("working", None);
    }
}

pub fn turn_finished(k: &Kernel) {
    let prev = k.working.fetch_sub(1, Ordering::Relaxed);
    if prev <= 1 {
        k.working.store(0, Ordering::Relaxed);
        if k.agent_status().0 == "working" {
            k.set_agent("ready", None);
        }
    }
}

/// Wait until the agent is ready (or fail with a message for the chat).
pub async fn wait_ready(k: &Kernel) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let (status, err) = k.agent_status();
        match status.as_str() {
            "ready" | "working" => return Ok(()),
            "unselected" => return Err("No provider is picked yet. Pick one to start.".into()),
            "starting" if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await
            }
            _ => {
                return Err(format!(
                    "The agent is not available ({status}){}",
                    err.map(|e| format!(": {e}")).unwrap_or_default()
                ));
            }
        }
    }
}

/// Plan steps as chat text.
pub fn plan_text(steps: &[(String, String)]) -> String {
    let mut text = String::from("Plan:");
    for (status, content) in steps {
        let mark = match status.as_str() {
            "completed" => "[x]",
            "in_progress" | "inProgress" => "[~]",
            _ => "[ ]",
        };
        text.push_str(&format!("\n{mark} {content}"));
    }
    text
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn gating_allows_only_release_providers() {
        assert_eq!(RELEASE_PROVIDERS, &["pi"]);
        for s in specs_with(Some("my-agent"), false) {
            assert!(allowed_with(false, &s.id), "{} must be allowed when not gated", s.id);
        }
        let gated: Vec<String> = specs_with(Some("my-agent"), true).into_iter().map(|s| s.id).filter(|id| allowed_with(true, id)).collect();
        assert_eq!(gated, vec!["pi"]);
        assert!(!specs_with(Some("my-agent"), true).iter().any(|s| s.id == "custom"), "custom is dropped when gated");
        assert!(specs_with(Some("my-agent"), false).iter().any(|s| s.id == "custom"));
    }

    #[tokio::test]
    async fn gated_detect_marks_others_coming_soon_without_probing() {
        // fake CLIs that leave a marker file if anything runs them
        let dir = std::env::temp_dir().join(format!("openapp-gating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        for bin in ["codex", "claude", "npx", "gemini", "goose", "opencode", "my-agent"] {
            let p = dir.join(bin);
            std::fs::write(&p, format!("#!/bin/sh\n/usr/bin/touch '{}'\necho ok\n", marker.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_env = dir.display().to_string();

        let list = detect_with(&path_env, Some("my-agent"), true).await;
        let ids: Vec<&str> = list.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["codex", "pi", "claude", "gemini", "goose", "opencode"], "no custom entry when gated");
        for h in &list {
            if h.id == "pi" {
                assert!(!h.coming_soon, "pi stays usable");
                assert_ne!(h.note.as_deref(), Some("Coming soon"));
            } else {
                assert!(h.coming_soon && !h.available && h.signed_in.is_none(), "{} must be inert", h.id);
                assert_eq!(h.note.as_deref(), Some("Coming soon"), "{}", h.id);
                assert!(h.login.is_none());
            }
        }
        assert!(!marker.exists(), "gated detection must not run any provider binary");
        let json = serde_json::to_value(&list[3]).unwrap();
        assert_eq!(json["comingSoon"], true);
        assert!(serde_json::to_value(list.iter().find(|h| h.id == "pi").unwrap()).unwrap().get("comingSoon").is_none());

        // control: the same PATH is probed when not gated
        let open = detect_with(&path_env, Some("my-agent"), false).await;
        assert!(open.iter().all(|h| !h.coming_soon));
        assert!(open.iter().any(|h| h.id == "gemini" && h.available), "ungated detection finds the CLIs on PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
