//! Snapshot types and persistence in `<workspace>/.openapp/` (contract §2, §8).

use crate::layout::Layout;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct ModuleInfo {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub name: String,
    /// Selected harness id, or null when none is picked.
    pub harness: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// §23 the model the next turn of the active session runs on (harness id; absent = the agent's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(rename = "modelName", skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// "default" | "strong"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub layout: Layout,
    pub theme: Map<String, Value>,
    pub modules: Vec<ModuleInfo>,
    pub views: BTreeMap<String, Value>,
    pub agent: AgentInfo,
    /// Chat of the active session.
    pub chat: Vec<Value>,
    /// Newest `updated` first.
    pub sessions: Vec<SessionMeta>,
    pub harnesses: Vec<crate::harness::HarnessInfo>,
    #[serde(rename = "activeSession")]
    pub active_session: String,
    /// Mode of the active session.
    #[serde(rename = "activeMode")]
    pub active_mode: String,
    /// Live state of web placements: placement id -> {url, title, loading} (host data `web:<placement>`).
    pub web: BTreeMap<String, Value>,
    /// §17 host data `grants`: what the user allowed (network hosts, microphone).
    pub grants: Vec<Value>,
    /// §19.1 presentations: [{id, view, as, title?, width?, after?}] (top = last).
    pub presented: Vec<Value>,
    /// §19.2 build turns of the active session: [{id, changes, modules, undone, undoable}].
    pub turns: Vec<Value>,
    /// §20.1 host data `secrets`: metadata only (never values).
    pub secrets: Vec<Value>,
    /// §20.3 guide strip, if shown.
    pub guide: Option<Value>,
    /// §21 host data of registry primitives (`{ "$host": name }` → `hostData[name]`).
    #[serde(rename = "hostData")]
    pub host_data: BTreeMap<String, Value>,
    /// §23 discovered models of every harness seen: [{id, displayName, description, efforts, defaultEffort, hidden, isDefault, harness}].
    pub models: Vec<Value>,
    /// §23 choices of the selected harness: {harness, chat, build, strong, policy, source, fetchedAt, error, escalated, current}.
    #[serde(rename = "modelPrefs")]
    pub model_prefs: Value,
}

/// Session as shown to the shell and the agent (§10).
#[derive(Debug, Clone, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created: String,
    pub updated: String,
    /// "chat" | "build"
    pub mode: String,
}

fn default_mode() -> String {
    "chat".into()
}

/// Session as stored in `<state>/sessions/index.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRec {
    pub id: String,
    pub title: String,
    pub created: String,
    pub updated: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Agent-side conversation id per harness id (ACP sessionId / Codex threadId), used for
    /// `session/load` / `thread/resume` after a restart.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agent_ids: BTreeMap<String, String>,
}

impl SessionRec {
    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            created: self.created.clone(),
            updated: self.updated.clone(),
            mode: self.mode.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionIndex {
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub sessions: Vec<SessionRec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistEntry {
    pub ts: String,
    pub layout: Layout,
    /// theme overrides (not resolved)
    pub theme: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModulesMeta {
    /// modules whose default placements were applied already
    #[serde(default)]
    pub defaults_applied: BTreeSet<String>,
}

pub const HISTORY_CAP: usize = 200;

/// `~/.openapp/state/<workspace-id>/` (`OPENAPP_STATE_HOME` overrides the root): layout, theme, history, research log,
/// sessions, harness choice, grants. Never inside the workspace, so the agent (VM or host) cannot edit grants or the
/// append-only research log. workspace-id = `ws-` + 16 hex of SHA-256 of the canonical workspace path.
pub fn state_dir(workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canon = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let h = Sha256::digest(canon.to_string_lossy().as_bytes());
    let id: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let root = match std::env::var_os("OPENAPP_STATE_HOME").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".openapp").join("state"),
    };
    root.join(format!("ws-{id}"))
}

/// Create the state dir and note which workspace it belongs to (`workspace.txt`, for people browsing the state root).
pub fn ensure_dir(workspace: &Path, state: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state)?;
    let _ = std::fs::write(state.join("workspace.txt"), workspace.to_string_lossy().as_bytes());
    Ok(())
}

pub struct Store {
    pub dir: PathBuf,
}

/// Write via a temp file + rename (readers never see a half-written file).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    std::fs::rename(tmp, path)
}

impl Store {
    /// `dir` = the kernel state dir (`state_dir(workspace)`).
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf() }
    }

    fn read_json<T: for<'de> Deserialize<'de> + Default>(&self, name: &str) -> T {
        let p = self.dir.join(name);
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                log::warn!("{}: invalid JSON ({e}); using defaults", p.display());
                T::default()
            }),
            Err(_) => T::default(),
        }
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) {
        let p = self.dir.join(name);
        let data = serde_json::to_vec_pretty(v).unwrap_or_default();
        if let Err(e) = write_atomic(&p, &data) {
            log::warn!("write {} failed: {e}", p.display());
        }
    }

    pub fn load_layout(&self) -> Layout {
        self.read_json("layout.json")
    }
    pub fn save_layout(&self, l: &Layout) {
        self.write_json("layout.json", l)
    }
    pub fn load_theme(&self) -> Map<String, Value> {
        self.read_json("theme.json")
    }
    pub fn save_theme(&self, t: &Map<String, Value>) {
        self.write_json("theme.json", t)
    }
    pub fn load_meta(&self) -> ModulesMeta {
        self.read_json("modules.json")
    }
    pub fn save_meta(&self, m: &ModulesMeta) {
        self.write_json("modules.json", m)
    }

    pub fn load_session_index(&self) -> SessionIndex {
        self.read_json("sessions/index.json")
    }
    pub fn save_session_index(&self, idx: &SessionIndex) {
        let _ = std::fs::create_dir_all(self.dir.join("sessions"));
        self.write_json("sessions/index.json", idx)
    }
    pub fn load_session_chat(&self, id: &str) -> Vec<Value> {
        let v: Value = self.read_json::<Option<Value>>(&format!("sessions/{id}.json")).unwrap_or(Value::Null);
        v.get("chat").and_then(Value::as_array).cloned().unwrap_or_default()
    }
    pub fn save_session_chat(&self, id: &str, chat: &[Value]) {
        let _ = std::fs::create_dir_all(self.dir.join("sessions"));
        self.write_json(&format!("sessions/{id}.json"), &serde_json::json!({ "id": id, "chat": chat }))
    }

    pub fn load_history(&self) -> Vec<HistEntry> {
        let p = self.dir.join("history.jsonl");
        let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
        let mut v: Vec<HistEntry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if v.len() > HISTORY_CAP {
            v.drain(..v.len() - HISTORY_CAP);
        }
        v
    }

    pub fn append_history(&self, e: &HistEntry) {
        let p = self.dir.join("history.jsonl");
        let line = serde_json::to_string(e).unwrap_or_default();
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            log::warn!("history append failed: {e}");
        }
    }

    pub fn rewrite_history(&self, entries: &[HistEntry]) {
        let mut out = String::new();
        for e in entries {
            out.push_str(&serde_json::to_string(e).unwrap_or_default());
            out.push('\n');
        }
        if let Err(e) = write_atomic(&self.dir.join("history.jsonl"), out.as_bytes()) {
            log::warn!("history rewrite failed: {e}");
        }
    }
}

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
