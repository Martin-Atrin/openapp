//! Append-only research log `<state>/research.jsonl` (outside the workspace; the agent cannot edit it) (contract §7).
//! Never log tokens, secret values or env values.
//!
//! One line: `{ ts, actor, kind, data }`. Every kind is a [`Kind`] (one list, below).
//!
//! **Actor rule: the actor is who caused the event.**
//! - `user`: the user did it (a prompt, a click, a card answer, a cancelled turn, a revoked grant).
//! - `agent`: the agent did it (an MCP tool call, a finished or failed agent turn, a card the agent asked for).
//! - `kernel`: OpenApp did it on its own (a card it shows, a VM start, a harness that exited, a turn it ended
//!   because the agent was not available).
//! - `module:<id>` / `view:<id>` / `host:<id>` / `agent:<id>`: that module (server, view, host part, agent module).
//!
//! When someone else asked first (e.g. a module asks for a secret, the user gives it), the actor is the one whose
//! action made the event happen (the user) and `data.by` names the requester.

use crate::util::MutexExt;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

macro_rules! kinds {
    ($($(#[doc = $doc:literal])* $v:ident = $s:literal,)*) => {
        /// Research log kinds.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Kind {
            $($(#[doc = $doc])* $v,)*
        }

        impl Kind {
            pub const ALL: &[Kind] = &[$(Kind::$v,)*];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Kind::$v => $s,)*
                }
            }
        }
    };
}

kinds! {
    // chat, sessions, turns
    /// a prompt the user sent
    Prompt = "prompt",
    SessionNew = "session_new",
    SessionOpen = "session_open",
    /// chat / build mode of a session
    Mode = "mode",
    /// the agent's full reply text of a turn
    AgentMessage = "agent_message",
    /// a tool call of the agent (MCP or harness tool)
    ToolCall = "tool_call",
    /// a turn ended: agent (reply / error), user (cancelled), kernel (agent not available, harness exited)
    TurnEnd = "turn_end",
    /// §23 the model a turn runs on
    TurnModel = "turn_model",
    /// question cards (§12.4): shown by the agent, answered by the user
    Question = "question",
    /// consent cards: shown (kernel or agent), decided (user)
    Permission = "permission",
    /// layout / theme undo (`/undo`, `openapp_undo`, turn.undo)
    Undo = "undo",
    // agent harness
    HarnessSelect = "harness_select",
    HarnessLogin = "harness_login",
    HarnessAccount = "harness_account",
    HarnessConfig = "harness_config",
    HarnessInstall = "harness_install",
    // §23 models
    ModelsDiscovered = "models_discovered",
    ModelChoice = "model_choice",
    ModelPolicy = "model_policy",
    ModelRequest = "model_request",
    ModelSwitch = "model_switch",
    // layout, theme, presentations, UI
    Layout = "layout",
    Theme = "theme",
    /// §19.1 view.open / view.close
    Present = "present",
    /// a view action (tool / host) from the shell
    UiAction = "ui_action",
    UiError = "ui_error",
    /// openapp_host_action
    HostAction = "host_action",
    /// a module action from the native shell (menu, tray, shortcut, notification, deep link)
    ShellAction = "shell_action",
    // modules
    ModuleLoad = "module_load",
    ModuleError = "module_error",
    /// §21 G9 lazy activation / idle stop / budgets / safe mode
    Activate = "activate",
    IdleStop = "idle_stop",
    Budget = "budget",
    SafeMode = "safe_mode",
    // app VM, network
    VmStart = "vm_start",
    VmError = "vm_error",
    VmImage = "vm_image",
    VmRemount = "vm_remount",
    /// a proxied connection from the VM
    Egress = "egress",
    // §15 / §18 host
    AudioRecord = "audio_record",
    HostService = "host_service",
    HostModuleBuild = "host_module_build",
    HostServiceStart = "host_service_start",
    HostServiceExit = "host_service_exit",
    GrantRevoke = "grant_revoke",
    // §20 connect
    Secret = "secret",
    NetFetch = "net_fetch",
    NetImap = "net_imap",
    Oauth = "oauth",
    WebControl = "web_control",
    Guide = "guide",
    Download = "download",
    // §21 primitives
    SecretRelease = "secret_release",
    ServicesStart = "services_start",
    ServicesStop = "services_stop",
    ServicesError = "services_error",
    ServiceExit = "service_exit",
    ServiceRestart = "service_restart",
    ServiceStop = "service_stop",
    AptInstall = "apt_install",
    LanExpose = "lan_expose",
    LanConnection = "lan_connection",
    PtyOpen = "pty_open",
    Window = "window",
    Menu = "menu",
    Tray = "tray",
    Shortcut = "shortcut",
    Notify = "notify",
    NotifyResponse = "notify_response",
    Clipboard = "clipboard",
    Share = "share",
    Drop = "drop",
    Deeplink = "deeplink",
    OpenFile = "open_file",
    FilesOpen = "files_open",
    FilesSave = "files_save",
    FolderGrant = "folder_grant",
    FolderRevoke = "folder_revoke",
    Job = "job",
    JobRun = "job_run",
    JobsWake = "jobs_wake",
    LoginItem = "login_item",
    AgentRun = "agent_run",
    AgentTool = "agent_tool",
}

/// Actor of an agent turn that ended with `stop_reason`: the user when they stopped it, else the agent.
pub fn turn_end_actor(stop_reason: &str) -> &'static str {
    if matches!(stop_reason, "cancelled" | "interrupted" | "aborted") { "user" } else { "agent" }
}

pub struct Research {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Research {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    /// `actor`: see the actor rule above.
    pub fn log(&self, actor: &str, kind: Kind, data: Value) {
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "actor": actor,
            "kind": kind.as_str(),
            "data": data,
        });
        let _g = self.lock.locked();
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            log::warn!("research log write failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_unique_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for k in Kind::ALL {
            let s = k.as_str();
            assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{s}");
            assert!(seen.insert(s), "duplicate kind {s}");
        }
        assert_eq!(turn_end_actor("cancelled"), "user");
        assert_eq!(turn_end_actor("end_turn"), "agent");
    }
}
