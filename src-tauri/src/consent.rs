//! Kernel consent cards: one path for every `permission` chat item the kernel shows.
//!
//! - [`open`] / [`close`]: the chat-card plumbing (append the card, register the waiter, mark it answered). Agent
//!   permission requests (pi.rs, codex.rs, acp.rs) use only these: their answer goes back to the agent.
//! - [`ask`]: a kernel-owned card with research logging (`permission` shown by the kernel, decided by the user) and an
//!   optional timeout.
//! - [`granted`]: Granted class (§21): the remembered decision in the grant store, else [`ask`]; the answer is stored.

use crate::kernel::{HostError, Kernel, PermWaiter};
use crate::research::Kind;
use crate::util::MutexExt;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// A consent card.
pub struct ConsentReq {
    /// research kind and grant kind (`network`, `microphone`, `host_module`, `lan`, …)
    pub kind: String,
    pub title: String,
    pub text: String,
    /// `[{ optionId, name, kind: allow_once|allow_always|reject_once|reject_always }]`
    pub options: Value,
    /// option ids that count as "allowed"
    pub allow: Vec<String>,
    /// extra card fields (`diff`, `details`)
    pub card: Map<String, Value>,
    /// extra research fields (module, host, …; never secret values)
    pub info: Map<String, Value>,
    /// default: the active session
    pub session: Option<String>,
    /// None: wait until answered (or the harness stops)
    pub timeout: Option<Duration>,
    /// Kernel cards are not cancelled when a turn ends (only when the harness stops).
    pub kernel_owned: bool,
}

impl ConsentReq {
    /// Allow / Deny (remembered), kernel-owned, no timeout.
    pub fn new(kind: &str, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            text: text.into(),
            options: json!([
                { "optionId": "allow", "name": "Allow", "kind": "allow_always" },
                { "optionId": "deny", "name": "Deny", "kind": "reject_always" }
            ]),
            allow: vec!["allow".into()],
            card: Map::new(),
            info: Map::new(),
            session: None,
            timeout: None,
            kernel_owned: true,
        }
    }

    pub fn options(mut self, options: Value, allow: &[&str]) -> Self {
        self.options = options;
        self.allow = allow.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn info(mut self, v: Value) -> Self {
        if let Value::Object(o) = v {
            self.info.extend(o);
        }
        self
    }

    pub fn card(mut self, key: &str, v: Value) -> Self {
        self.card.insert(key.into(), v);
        self
    }

    pub fn session(mut self, sid: &str) -> Self {
        self.session = Some(sid.into());
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Not kernel-owned: cancelled with the agent turn of its session.
    pub fn turn_owned(mut self) -> Self {
        self.kernel_owned = false;
        self
    }
}

pub struct Decision {
    /// chosen option id (None = dismissed, timed out or cancelled)
    pub choice: Option<String>,
    pub allowed: bool,
}

/// Append a `permission` card to a session and register its waiter. The receiver yields the chosen option id
/// (None when the card is cancelled).
pub fn open(k: &Kernel, sid: &str, mut card: Value, kernel_owned: bool) -> (String, oneshot::Receiver<Option<String>>) {
    card["kind"] = json!("permission");
    let item = k.chat_append_to(sid, card);
    let (tx, rx) = oneshot::channel();
    k.perms.locked().insert(item.clone(), PermWaiter { tx, session: sid.to_string(), kernel_owned });
    (item, rx)
}

/// Mark a card answered (`resolved` = the option id, or null).
pub fn close(k: &Kernel, sid: &str, item: &str, choice: &Option<String>) {
    let mut patch = Map::new();
    patch.insert("resolved".into(), json!(choice));
    k.chat_update_in(sid, item, patch);
}

/// Show a kernel consent card and wait for the user.
pub async fn ask(k: &Kernel, req: ConsentReq) -> Decision {
    let sid = req.session.clone().unwrap_or_else(|| k.active_session());
    let mut card = json!({ "title": req.title, "text": req.text, "options": req.options });
    if let Some(o) = card.as_object_mut() {
        o.extend(req.card);
    }
    let (item, rx) = open(k, &sid, card, req.kernel_owned);
    let mut shown = json!({ "phase": "shown", "kind": req.kind, "id": item, "session": sid });
    if let Some(o) = shown.as_object_mut() {
        o.extend(req.info.clone());
    }
    k.research.log("kernel", Kind::Permission, shown);
    let t0 = Instant::now();
    let choice = match req.timeout {
        None => rx.await.unwrap_or(None),
        Some(d) => match tokio::time::timeout(d, rx).await {
            Ok(r) => r.unwrap_or(None),
            Err(_) => {
                k.perms.locked().remove(&item);
                None
            }
        },
    };
    close(k, &sid, &item, &choice);
    let allowed = choice.as_ref().is_some_and(|c| req.allow.contains(c));
    let mut decided = json!({ "phase": "decided", "kind": req.kind, "id": item, "option": choice, "allowed": allowed, "ms": t0.elapsed().as_millis() as u64 });
    if let Some(o) = decided.as_object_mut() {
        o.extend(req.info);
    }
    k.research.log("user", Kind::Permission, decided);
    Decision { choice, allowed }
}

pub struct Granted {
    pub allowed: bool,
    /// a card was shown (false: the stored decision answered)
    pub asked: bool,
}

/// Granted class: the stored decision for `kind` / `target`, else a card whose answer is stored (a denial only with
/// `remember_deny`; a dismissed card stores nothing). `extra` goes into the stored grant (e.g. `actor`).
pub async fn granted(k: &Kernel, target: &str, req: ConsentReq, remember_deny: bool, extra: Value) -> Granted {
    if let Some(allowed) = k.grants.allowed(&req.kind, target) {
        return Granted { allowed, asked: false };
    }
    let kind = req.kind.clone();
    let d = ask(k, req).await;
    if d.choice.is_some() && (d.allowed || remember_deny) {
        k.grants.set(&kind, target, d.allowed, extra);
        k.emit_snapshot();
    }
    Granted { allowed: d.allowed, asked: true }
}

/// Granted-class consent per module with the standard messages (`<kind> for "<module>" was denied …`; Forbidden).
pub async fn module_grant(k: &Kernel, kind: &str, module: &str, title: &str, text: &str) -> Result<(), HostError> {
    let g = granted(k, module, ConsentReq::new(kind, title, text).info(json!({ "module": module })), true, Value::Null).await;
    match g {
        Granted { allowed: true, .. } => Ok(()),
        Granted { asked: false, .. } => Err(HostError::forbidden(format!("{kind} for \"{module}\" was denied (the user can revoke the decision in grants: {kind}:{module})"))),
        Granted { asked: true, .. } => Err(HostError::forbidden(format!("{kind} for \"{module}\" was not allowed"))),
    }
}
