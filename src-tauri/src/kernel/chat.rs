//! Chat, sessions, turns, questions and permission answers; the agent status and harness cards.

use crate::research::Kind;
use super::*;

pub(crate) const CHAT_CAP: usize = 1000;

pub struct SessionState {
    pub rec: SessionRec,
    /// Loaded lazily from `<state>/sessions/<id>.json`.
    pub chat: Option<Vec<Value>>,
    pub dirty: bool,
}

pub const DEFAULT_SESSION_TITLE: &str = "New session";

pub(crate) const TITLE_MAX: usize = 48;

pub(crate) fn new_session_id() -> String {
    format!("s_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

pub(crate) fn new_session_rec() -> SessionRec {
    let now = now_ts();
    SessionRec { id: new_session_id(), title: DEFAULT_SESSION_TITLE.into(), created: now.clone(), updated: now, mode: "chat".into(), agent_ids: BTreeMap::new() }
}

/// First line of a prompt, at most 48 chars.
pub fn title_from_prompt(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    if line.chars().count() > TITLE_MAX {
        format!("{}…", line.chars().take(TITLE_MAX - 1).collect::<String>().trim_end())
    } else {
        line
    }
}

impl Inner {
    /// The chat of a session (loading it from disk on first use).
    pub(crate) fn chat_mut(&mut self, store: &Store, sid: &str) -> Option<&mut Vec<Value>> {
        let s = self.sessions.get_mut(sid)?;
        if s.chat.is_none() {
            s.chat = Some(store.load_session_chat(sid));
        }
        s.chat.as_mut()
    }

    pub(crate) fn active_chat(&self) -> &[Value] {
        self.sessions.get(&self.active).and_then(|s| s.chat.as_deref()).unwrap_or(&[])
    }

    pub fn session_metas(&self) -> Vec<SessionMeta> {
        let mut v: Vec<SessionMeta> = self.sessions.values().map(|s| s.rec.meta()).collect();
        v.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.created.cmp(&a.created)));
        v
    }

    pub(crate) fn index(&self) -> SessionIndex {
        let mut sessions: Vec<SessionRec> = self.sessions.values().map(|s| s.rec.clone()).collect();
        sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
        SessionIndex { active: Some(self.active.clone()), sessions }
    }
}

pub struct QuestionWaiter {
    /// None = cancelled
    pub tx: oneshot::Sender<Option<Map<String, Value>>>,
    pub session: String,
    pub started: std::time::Instant,
    /// question ids whose answers must not be shown or logged
    pub secret: Vec<String>,
}

/// §19.2: marks a session's turn as running (stamps chat items with turnId/mode) until dropped.
pub struct TurnGuard(Arc<Kernel>, String);

impl TurnGuard {
    pub fn begin(k: &Arc<Kernel>, sid: &str) -> Self {
        k.turn_begin(sid);
        TurnGuard(k.clone(), sid.to_string())
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.turn_end(&self.1);
    }
}

/// §19.2: what one build-mode turn changed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TurnRec {
    pub session: String,
    pub mode: String,
    /// layout/theme before the first change of this turn
    #[serde(skip)]
    pub before: Option<HistEntry>,
    pub changes: usize,
    /// modules first loaded in this turn
    pub modules: Vec<String>,
    pub undone: bool,
    pub ts: String,
}

pub struct PermWaiter {
    pub tx: oneshot::Sender<Option<String>>,
    /// OpenApp session the request belongs to.
    pub session: String,
    /// Kernel capability cards (network, microphone) are not tied to an agent turn: they are not
    /// cancelled when a turn ends, only when the harness stops.
    pub kernel_owned: bool,
}

impl Kernel {
    /// Called by the harness adapters when a session's turn really starts (after its turn lock).
    pub fn turn_begin(&self, sid: &str) {
        let next = self.turn_queue.locked().get_mut(sid).and_then(|q| q.pop_front());
        let Some((tid, mode)) = next else { return };
        self.turns.locked().insert(
            tid.clone(),
            TurnRec { session: sid.to_string(), mode: mode.clone(), ts: now_ts(), ..Default::default() },
        );
        self.turn_now.locked().insert(sid.to_string(), (tid, mode));
    }

    pub fn turn_end(&self, sid: &str) {
        let ended = self.turn_now.locked().remove(sid);
        if let Some((tid, _)) = ended {
            // keep only build turns that changed something (and the last 200)
            let mut t = self.turns.locked();
            if t.get(&tid).is_some_and(|r| r.changes == 0 && r.modules.is_empty()) {
                t.remove(&tid);
            }
            while t.len() > 200 {
                let k = t.keys().next().cloned().unwrap_or_default();
                t.remove(&k);
            }
        }
        self.emit_snapshot();
    }

    /// §23 the session whose turn is running (the active one if several, else the most recent); None when idle.
    pub fn running_session(&self) -> Option<String> {
        let active = self.active_session();
        let now = self.turn_now.locked();
        if now.is_empty() {
            return None;
        }
        if now.contains_key(&active) {
            return Some(active);
        }
        let t = self.turns.locked();
        now.iter().filter_map(|(s, (id, _))| t.get(id).map(|r| (r.ts.clone(), s.clone()))).max().map(|(_, s)| s).or_else(|| now.keys().next().cloned())
    }

    /// The running build-mode turn that MCP changes are attributed to (most recent if several).
    pub(crate) fn build_turn(&self) -> Option<String> {
        let now = self.turn_now.locked();
        let t = self.turns.locked();
        now.values().filter(|(_, m)| m == "build").filter_map(|(id, _)| t.get(id).map(|r| (r.ts.clone(), id.clone()))).max().map(|(_, id)| id)
    }

    /// Host action `turn.undo {turnId?}`: revert the layout/theme changes of a build turn and unload
    /// (and unplace) the modules first loaded in it. Module files stay in the workspace.
    pub async fn turn_undo(self: &Arc<Self>, turn_id: Option<&str>) -> Result<Value, String> {
        let active = self.active_session();
        let (tid, rec) = {
            let t = self.turns.locked();
            let found = match turn_id {
                Some(id) => t.get(id).map(|r| (id.to_string(), r.clone())),
                None => t.iter().rev().find(|(_, r)| r.session == active && !r.undone && r.mode == "build").map(|(k, r)| (k.clone(), r.clone())),
            };
            found.ok_or_else(|| format!("no build turn {} to undo", turn_id.unwrap_or("(latest)")))?
        };
        if rec.undone {
            return Err("this turn was already undone".into());
        }
        if rec.before.is_none() && rec.modules.is_empty() {
            return Err("this turn changed nothing that can be undone".into());
        }
        let mut later = 0;
        if let Some(before) = &rec.before {
            let mut g = self.lock();
            later = g.history.iter().filter(|h| h.ts > before.ts).count().saturating_sub(rec.changes);
            self.push_history(&mut g);
            g.layout = before.layout.clone();
            g.theme = before.theme.clone();
            self.store.save_layout(&g.layout);
            self.store.save_theme(&g.theme);
        }
        for m in &rec.modules {
            let _ = self.module_unload(m, true).await;
        }
        if let Some(r) = self.turns.locked().get_mut(&tid) {
            r.undone = true;
        }
        let note = if rec.modules.is_empty() { String::new() } else { format!(" Module files are kept in the workspace ({}).", rec.modules.iter().map(|m| format!("modules/{m}/")).collect::<Vec<_>>().join(", ")) };
        let later_note = if later > 0 { format!(" {later} later layout/theme change(s) were reverted too.") } else { String::new() };
        self.system_message_in(&rec.session, "system", &format!("Undid this build: {} layout/theme change(s), {} module(s) unloaded.{note}{later_note}", rec.changes, rec.modules.len()));
        self.research.log("user", Kind::Undo, json!({ "turn": tid, "changes": rec.changes, "modules": rec.modules, "later_reverted": later }));
        self.emit_snapshot();
        Ok(json!({ "ok": true, "turn": tid, "reverted_changes": rec.changes, "unloaded_modules": rec.modules, "later_reverted": later, "files_kept": true }))
    }

    /// Replace the detected harness list (live overlays are kept).
    pub fn set_harnesses(&self, list: Vec<crate::harness::HarnessInfo>) {
        *self.harnesses.locked() = list;
        self.apply_overlays();
    }

    pub(crate) fn apply_overlays(&self) {
        let ov = self.harness_overlay.locked().clone();
        let mut list = self.harnesses.locked();
        for h in list.iter_mut().filter(|h| !h.coming_soon) {
            if let Some(o) = ov.get(&h.id) {
                if o.signed_in.is_some() {
                    h.signed_in = o.signed_in;
                }
                if o.note.is_some() {
                    h.note = o.note.clone();
                }
                if let (Some(m), true) = (&o.model_note, h.signed_in != Some(false)) {
                    // §23: the card shows the discovered model
                    h.note = Some(match h.note.take() {
                        Some(n) if !n.is_empty() => format!("{n} · {m}"),
                        _ => m.clone(),
                    });
                }
                h.login = o.login.clone();
                h.accounts = o.accounts.clone();
            }
        }
    }

    /// Change the live state of a harness card and push a snapshot.
    pub fn update_harness(&self, id: &str, f: impl FnOnce(&mut crate::harness::Overlay)) {
        {
            let mut ov = self.harness_overlay.locked();
            f(ov.entry(id.to_string()).or_default());
        }
        self.apply_overlays();
        self.emit_snapshot();
    }

    pub fn harness_overlay(&self, id: &str) -> crate::harness::Overlay {
        self.harness_overlay.locked().get(id).cloned().unwrap_or_default()
    }

    pub fn set_agent(&self, status: &str, error: Option<String>) {
        {
            let mut g = self.lock();
            g.agent.status = status.into();
            g.agent.error = error;
        }
        self.emit_snapshot();
    }

    pub fn set_agent_name(&self, name: &str) {
        self.lock().agent.name = name.into();
    }

    pub fn agent_status(&self) -> (String, Option<String>) {
        let g = self.lock();
        (g.agent.status.clone(), g.agent.error.clone())
    }

    pub fn active_session(&self) -> String {
        self.lock().active.clone()
    }

    /// Append a chat item to the active session. Returns the item id.
    pub fn chat_append(&self, item: Value) -> String {
        let sid = self.active_session();
        self.chat_append_to(&sid, item)
    }

    /// Append a chat item to a session (assigns `id` when missing). `oa://chat` is emitted only
    /// for the active session; other sessions show up in the snapshot when opened.
    pub fn chat_append_to(&self, sid: &str, mut item: Value) -> String {
        // §19.2: items of a running turn carry its turnId and mode
        if item.get("turnId").is_none()
            && let Some((t, m)) = self.turn_now.locked().get(sid).cloned() {
                item["turnId"] = json!(t);
                item["mode"] = json!(m);
            }
        let id = match item.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => {
                let id = format!("c_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
                item["id"] = json!(id);
                id
            }
        };
        let is_active = {
            let mut g = self.lock();
            let Some(chat) = g.chat_mut(&self.store, sid) else { return id };
            chat.push(item.clone());
            if chat.len() > CHAT_CAP {
                let n = chat.len() - CHAT_CAP;
                chat.drain(..n);
            }
            if let Some(s) = g.sessions.get_mut(sid) {
                s.dirty = true;
            }
            g.active == sid
        };
        if is_active {
            self.sink.emit("oa://chat", json!({ "op": "append", "item": item }));
        }
        id
    }

    /// Merge `patch` into a chat item and emit an update (active session only).
    pub fn chat_update_in(&self, sid: &str, id: &str, patch: Map<String, Value>) {
        let is_active = {
            let mut g = self.lock();
            let Some(chat) = g.chat_mut(&self.store, sid) else { return };
            if let Some(item) = chat.iter_mut().rev().find(|i| i.get("id").and_then(Value::as_str) == Some(id))
                && let Some(o) = item.as_object_mut() {
                    for (k, v) in &patch {
                        o.insert(k.clone(), v.clone());
                    }
                }
            if let Some(s) = g.sessions.get_mut(sid) {
                s.dirty = true;
            }
            g.active == sid
        };
        if is_active {
            self.sink.emit("oa://chat", json!({ "op": "update", "id": id, "patch": patch }));
        }
    }

    /// Append text to a chat item's `text`; the update carries the FULL text.
    pub fn chat_append_text_in(&self, sid: &str, id: &str, delta: &str) {
        let (full, is_active) = {
            let mut g = self.lock();
            let Some(chat) = g.chat_mut(&self.store, sid) else { return };
            let Some(item) = chat.iter_mut().rev().find(|i| i.get("id").and_then(Value::as_str) == Some(id)) else {
                return;
            };
            let mut t = item.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            t.push_str(delta);
            item["text"] = json!(t);
            if let Some(s) = g.sessions.get_mut(sid) {
                s.dirty = true;
            }
            (t, g.active == sid)
        };
        if is_active {
            let mut patch = Map::new();
            patch.insert("text".into(), json!(full));
            self.sink.emit("oa://chat", json!({ "op": "update", "id": id, "patch": patch }));
        }
    }

    /// Text of a chat item in a session.
    pub fn chat_item(&self, sid: &str, id: &str) -> Option<Value> {
        let mut g = self.lock();
        g.chat_mut(&self.store, sid)?.iter().rev().find(|i| i["id"] == id).cloned()
    }

    pub fn system_message_in(&self, sid: &str, kind: &str, text: &str) {
        self.chat_append_to(sid, json!({ "kind": kind, "text": text }));
    }

    /// Write dirty session chats (the watcher calls this every 500 ms, which debounces streaming).
    pub fn flush_sessions(&self) {
        let dirty: Vec<(String, Vec<Value>)> = {
            let mut g = self.lock();
            g.sessions
                .values_mut()
                .filter(|s| s.dirty)
                .filter_map(|s| {
                    s.dirty = false;
                    Some((s.rec.id.clone(), s.chat.clone()?))
                })
                .collect()
        };
        for (id, chat) in dirty {
            self.store.save_session_chat(&id, &chat);
        }
    }

    pub(crate) fn save_index(&self) {
        let idx = self.lock().index();
        self.store.save_session_index(&idx);
    }

    /// Mark a session as updated now (and persist index + chat).
    pub fn touch_session(&self, sid: &str) {
        {
            let mut g = self.lock();
            if let Some(s) = g.sessions.get_mut(sid) {
                s.rec.updated = now_ts();
            }
        }
        self.save_index();
        self.flush_sessions();
        self.emit_snapshot();
    }

    pub fn session_rec(&self, sid: &str) -> Option<SessionRec> {
        self.lock().sessions.get(sid).map(|s| s.rec.clone())
    }

    /// Agent-side conversation id of a session for a harness.
    pub fn agent_session_id(&self, sid: &str, harness: &str) -> Option<String> {
        self.lock().sessions.get(sid).and_then(|s| s.rec.agent_ids.get(harness).cloned())
    }

    pub fn set_agent_session_id(&self, sid: &str, harness: &str, id: Option<String>) {
        {
            let mut g = self.lock();
            if let Some(s) = g.sessions.get_mut(sid) {
                match id {
                    Some(id) => {
                        s.rec.agent_ids.insert(harness.to_string(), id);
                    }
                    None => {
                        s.rec.agent_ids.remove(harness);
                    }
                }
            }
        }
        self.save_index();
    }

    /// Create a session and make it active. An empty, untitled active session is reused.
    pub fn session_new(&self) -> String {
        let (id, reused) = {
            let mut g = self.lock();
            let active = g.active.clone();
            let reuse = g
                .sessions
                .get(&active)
                .is_some_and(|s| s.rec.title == DEFAULT_SESSION_TITLE && s.chat.as_ref().is_some_and(|c| c.is_empty()));
            if reuse {
                (active, true)
            } else {
                let rec = new_session_rec();
                let id = rec.id.clone();
                g.sessions.insert(id.clone(), SessionState { rec, chat: Some(Vec::new()), dirty: true });
                g.active = id.clone();
                (id, false)
            }
        };
        self.save_index();
        self.flush_sessions();
        self.research.log("user", Kind::SessionNew, json!({ "id": id, "reused_empty": reused }));
        self.emit_snapshot();
        id
    }

    pub fn session_open(&self, id: &str) -> Result<(), String> {
        {
            let mut g = self.lock();
            if !g.sessions.contains_key(id) {
                return Err(format!("no session \"{id}\""));
            }
            g.chat_mut(&self.store, id);
            g.active = id.to_string();
        }
        self.save_index();
        self.research.log("user", Kind::SessionOpen, json!({ "id": id }));
        self.emit_snapshot();
        Ok(())
    }

    pub fn session_set_mode(&self, id: &str, mode: &str) -> Result<(), String> {
        if mode != "chat" && mode != "build" {
            return Err(format!("mode must be \"chat\" or \"build\", got \"{mode}\""));
        }
        {
            let mut g = self.lock();
            let s = g.sessions.get_mut(id).ok_or_else(|| format!("no session \"{id}\""))?;
            s.rec.mode = mode.into();
        }
        self.save_index();
        self.research.log("user", Kind::Mode, json!({ "session": id, "mode": mode }));
        self.emit_snapshot();
        Ok(())
    }

    pub fn session_rename(&self, id: &str, title: &str) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("title must not be empty".into());
        }
        let title: String = title.chars().take(120).collect();
        {
            let mut g = self.lock();
            let s = g.sessions.get_mut(id).ok_or_else(|| format!("no session \"{id}\""))?;
            s.rec.title = title;
        }
        self.save_index();
        self.emit_snapshot();
        Ok(())
    }

    pub fn system_message(&self, kind: &str, text: &str) {
        self.chat_append(json!({ "kind": kind, "text": text }));
    }

    pub fn send_prompt(self: &Arc<Self>, text: String) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        if t.starts_with('/') && crate::primitives::slash(self, t) {
            return;
        }
        if let Some(rest) = t.strip_prefix("/undo")
            && (rest.is_empty() || rest.starts_with(' ')) {
                let steps = rest.trim().parse::<usize>().unwrap_or(1).max(1);
                self.chat_append(json!({ "kind": "user", "text": t }));
                let r = self.undo(steps, "user");
                let msg = match r.get("undone").and_then(Value::as_u64) {
                    Some(n) if n > 0 => format!("Undid {n} layout/theme change{}.", if n == 1 { "" } else { "s" }),
                    _ => "Nothing to undo.".to_string(),
                };
                self.system_message("system", &msg);
                return;
            }
        if t == "/chat" {
            let sid = self.active_session();
            let _ = self.session_set_mode(&sid, "chat");
            self.system_message("system", "Chat mode: the agent answers but does not change the app. Type /build to build.");
            return;
        }
        if t == "/build" || t.starts_with("/build ") {
            let sid = self.active_session();
            let _ = self.session_set_mode(&sid, "build");
            let rest = t["/build".len()..].trim().to_string();
            if rest.is_empty() {
                self.system_message("system", "Build mode: the agent changes the app. Type /chat to just talk.");
                return;
            }
            return self.send_prompt(rest);
        }
        if t == "/account" || t == "/accounts" {
            crate::primitives::shell::emit_shell(self, json!({ "op": "accounts" }));
            return;
        }
        if t == "/provider" {
            crate::harness::unselect(self);
            self.system_message("system", "Pick a provider to continue.");
            return;
        }
        if let Some(rest) = t.strip_prefix("/reset")
            && rest.trim().is_empty() {
                self.reset();
                self.system_message("system", "Reset: the canvas is empty again and the theme is back to default. Modules are kept in the workspace.");
                return;
            }
        let sid = self.active_session();
        {
            let mut g = self.lock();
            if let Some(s) = g.sessions.get_mut(&sid)
                && s.rec.title == DEFAULT_SESSION_TITLE {
                    s.rec.title = title_from_prompt(t);
                }
        }
        let build = self.session_rec(&sid).is_some_and(|r| r.mode == "build");
        let mode = if build { "build" } else { "chat" };
        let turn_id = format!("t_{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        self.turn_queue.locked().entry(sid.clone()).or_default().push_back((turn_id.clone(), mode.into()));
        self.chat_append_to(&sid, json!({ "kind": "user", "text": text, "turnId": turn_id, "mode": mode }));
        self.touch_session(&sid);
        self.research.log("user", Kind::Prompt, json!({ "session": sid, "text": text, "turn": turn_id }));
        let k = self.clone();
        tokio::spawn(async move { crate::harness::run_turn(k, sid, text, build).await });
    }

    pub fn cancel_turn(&self) {
        crate::harness::cancel(self);
    }

    /// Agent asks the user (§12.4): adds a `question` chat item; the receiver yields the answers
    /// (`{questionId: [string]}`) or None when cancelled.
    pub fn ask_questions(&self, sid: &str, questions: Vec<Value>, source: &str) -> (String, oneshot::Receiver<Option<Map<String, Value>>>) {
        let secret: Vec<String> = questions
            .iter()
            .filter(|q| q["isSecret"].as_bool() == Some(true))
            .filter_map(|q| q["id"].as_str().map(String::from))
            .collect();
        let id = self.chat_append_to(sid, json!({ "kind": "question", "questions": questions }));
        let (tx, rx) = oneshot::channel();
        self.questions.locked().insert(
            id.clone(),
            QuestionWaiter { tx, session: sid.to_string(), started: std::time::Instant::now(), secret },
        );
        self.research.log("agent", Kind::Question, json!({ "phase": "shown", "session": sid, "id": id, "source": source, "count": questions.len() }));
        (id, rx)
    }

    /// Shell command `answer_question` (None = cancel).
    pub fn answer_question(&self, request_id: &str, answers: Option<Map<String, Value>>) -> Result<(), String> {
        let w = self
            .questions
            .locked()
            .remove(request_id)
            .ok_or_else(|| format!("no open question \"{request_id}\""))?;
        let shown = answers.as_ref().map(|a| {
            let mut m = a.clone();
            for s in &w.secret {
                if m.contains_key(s) {
                    m.insert(s.clone(), json!(["••••"]));
                }
            }
            Value::Object(m)
        });
        let mut patch = Map::new();
        patch.insert("resolved".into(), shown.unwrap_or(Value::Null));
        self.chat_update_in(&w.session, request_id, patch);
        self.research.log(
            "user",
            Kind::Question,
            json!({ "phase": "answered", "id": request_id, "answered": answers.is_some(), "count": answers.as_ref().map(|a| a.len()), "ms": w.started.elapsed().as_millis() as u64 }),
        );
        let _ = w.tx.send(answers);
        Ok(())
    }

    pub fn respond_permission(&self, request_id: &str, option_id: Option<String>) -> Result<(), String> {
        let w = self.perms.locked().remove(request_id);
        match w {
            Some(w) => {
                let _ = w.tx.send(option_id);
                Ok(())
            }
            None => Err(format!("no open permission request \"{request_id}\"")),
        }
    }
}
