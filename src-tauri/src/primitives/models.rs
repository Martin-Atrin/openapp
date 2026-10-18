//! §23 Models are discovered, never hardcoded.
//!
//! - Discovery per harness (after start/sign-in, `models.refresh`, daily): Codex `model/list` (codex.rs),
//!   Pi `/openapp-models` (pi.rs + workspace-template/pi/openapp.ts: Pi's available models filtered by the
//!   account's live list), ACP `session/new` models / config options (acp.rs). The adapters call [`set_catalog`].
//! - Catalog `{id, displayName, description, efforts[], defaultEffort, hidden, isDefault, harness}` in the snapshot
//!   (`models`), describe (`models`) and host data `models`; changes (added/removed) are research-logged.
//! - Choice per harness: `chat`, `build` (falls back to chat) and `strong` slots of `{model, effort}` in
//!   `<state>/models.json`. No choice → the harness default (catalog `isDefault`, else the first non-hidden).
//!   A choice whose model left the catalog is dropped with one system line.
//! - Efforts: one ladder (none|off < minimal < low < medium < high < xhigh < max < ultra); a wanted effort is mapped
//!   to the nearest one the model supports (Codex and Pi name the steps differently: `none` = `off`).
//! - Escalation: `openapp_model_request {tier, reason}` → policy `ask` (kernel card) or `auto`; the session runs on
//!   the strong slot until its turn (and the follow-up turn the kernel starts for Codex/ACP) ends, then reverts.
//! - `/model [chat|build|strong] <name> [effort]`, `/model policy ask|auto`, `/model refresh`, `/model default`.

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

/// Effort ladder (rank = index; `none` and `off` share rank 0).
const LADDER: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const DAY_SECS: u64 = 24 * 3600;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// Harness-native id: Codex `model`, Pi `provider/id`, ACP model id / config value.
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    /// Efforts in the harness's own vocabulary, lowest first.
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(rename = "defaultEffort", default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
    pub harness: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Catalog {
    models: Vec<ModelInfo>,
    source: String,
    #[serde(default)]
    fetched_at: String,
    #[serde(default)]
    fetched_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Prefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    build: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strong: Option<Choice>,
}

impl Prefs {
    fn slot_mut(&mut self, slot: &str) -> &mut Option<Choice> {
        match slot {
            "build" => &mut self.build,
            "strong" => &mut self.strong,
            _ => &mut self.chat,
        }
    }
    fn slot(&self, slot: &str) -> Option<&Choice> {
        match slot {
            "build" => self.build.as_ref(),
            "strong" => self.strong.as_ref(),
            _ => self.chat.as_ref(),
        }
    }
}

fn default_policy() -> String {
    "ask".into()
}

/// `<state>/models.json`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    #[serde(default)]
    catalogs: BTreeMap<String, Catalog>,
    #[serde(default)]
    prefs: BTreeMap<String, Prefs>,
    #[serde(default = "default_policy")]
    policy: String,
    #[serde(default, rename = "includeHidden")]
    include_hidden: bool,
    /// Models the provider refused for this account (removed from later discoveries).
    #[serde(default)]
    rejected: BTreeMap<String, BTreeSet<String>>,
}

struct Escalation {
    harness: String,
    choice: Choice,
    reason: String,
    /// Codex/ACP: the kernel starts a follow-up turn on the strong model when this turn ends.
    followup: bool,
}

#[derive(Default)]
struct St {
    store: Mutex<Option<Store>>,
    esc: Mutex<HashMap<String, Escalation>>,
    refreshing: Mutex<BTreeSet<String>>,
    /// Harnesses whose next discovery should announce the default (after a sign-in).
    announce: Mutex<BTreeSet<String>>,
}

fn st(k: &Kernel) -> Arc<St> {
    k.prims.get::<St>()
}

fn path(k: &Kernel) -> std::path::PathBuf {
    k.state_dir.join("models.json")
}

/// Read (or change, then persist) the store. Never call kernel locks or emit inside `f`.
fn read<R>(k: &Kernel, f: impl FnOnce(&Store) -> R) -> R {
    let s = st(k);
    let mut g = s.store.locked();
    let store = g.get_or_insert_with(|| load(k));
    f(store)
}

fn write<R>(k: &Kernel, f: impl FnOnce(&mut Store) -> R) -> R {
    let s = st(k);
    let mut g = s.store.locked();
    let store = g.get_or_insert_with(|| load(k));
    let r = f(store);
    if let Ok(text) = serde_json::to_string_pretty(store) {
        let p = path(k);
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
            log::warn!("models: could not write {}", p.display());
        }
    }
    r
}

fn load(k: &Kernel) -> Store {
    let mut s: Store = std::fs::read(path(k)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if s.policy != "auto" {
        s.policy = "ask".into();
    }
    s
}

// ---------- efforts ----------

fn rank(e: &str) -> Option<usize> {
    let e = e.to_ascii_lowercase();
    if e == "none" {
        return Some(0);
    }
    LADDER.iter().position(|l| *l == e)
}

/// `xhigh` → "Extra high".
pub fn effort_label(e: &str) -> String {
    match e.to_ascii_lowercase().as_str() {
        "none" | "off" => "Off".into(),
        "xhigh" => "Extra high".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

/// A typed effort ("extra high", "x-high", "xhigh", "Max") → ladder name.
pub fn parse_effort(s: &str) -> Option<String> {
    let n: String = s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let v = match n.as_str() {
        "none" | "off" | "no" => "off",
        "minimal" | "min" => "minimal",
        "low" | "lo" => "low",
        "medium" | "med" | "mid" | "normal" => "medium",
        "high" | "hi" => "high",
        "xhigh" | "extrahigh" | "veryhigh" | "xh" => "xhigh",
        "max" | "maximum" => "max",
        "ultra" => "ultra",
        _ => return None,
    };
    Some(v.into())
}

/// The supported effort closest to `want` (exact, else nearest rank; ties go lower). Keeps the harness's spelling.
pub fn nearest_effort(want: &str, supported: &[String]) -> Option<String> {
    if let Some(e) = supported.iter().find(|e| e.eq_ignore_ascii_case(want)) {
        return Some(e.clone());
    }
    let w = rank(want)?;
    supported
        .iter()
        .filter_map(|e| rank(e).map(|r| (r.abs_diff(w), r, e)))
        .min_by_key(|(d, r, _)| (*d, *r))
        .map(|(_, _, e)| e.clone())
}

// ---------- catalog ----------

pub fn catalog(k: &Kernel, harness: &str) -> Vec<ModelInfo> {
    read(k, |s| s.catalogs.get(harness).map(|c| c.models.clone()).unwrap_or_default())
}

/// Cached catalog ids of another harness (Pi cross-checks a ChatGPT sign-in against Codex's account list).
pub fn cached_catalog(k: &Kernel, harness: &str) -> Vec<ModelInfo> {
    catalog(k, harness)
}

pub fn include_hidden(k: &Kernel) -> bool {
    read(k, |s| s.include_hidden)
}

fn default_of(cat: &[ModelInfo]) -> Option<&ModelInfo> {
    cat.iter().find(|m| m.is_default && !m.hidden).or_else(|| cat.iter().find(|m| !m.hidden)).or_else(|| cat.first())
}

fn display(cat: &[ModelInfo], id: &str) -> String {
    cat.iter().find(|m| m.id == id).map(|m| m.display_name.clone()).unwrap_or_else(|| id.to_string())
}

/// Fuzzy match on displayName / id: exact, normalized-equal, all query tokens contained, then subsequence.
/// Ties keep the catalog order (harnesses list newer models first).
pub fn find_model<'a>(cat: &'a [ModelInfo], query: &str) -> Option<&'a ModelInfo> {
    let norm = |s: &str| s.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let nq = norm(q);
    let tokens: Vec<String> = q.split(|c: char| !c.is_ascii_alphanumeric() && c != '.').filter(|t| !t.is_empty()).map(norm).filter(|t| !t.is_empty()).collect();
    let names = |m: &ModelInfo| [m.display_name.clone(), m.id.clone(), m.id.rsplit('/').next().unwrap_or("").to_string()];
    let score = |m: &ModelInfo| -> u32 {
        let mut best = 0;
        for n in names(m) {
            let nn = norm(&n);
            let s = if n.eq_ignore_ascii_case(q) {
                100
            } else if nn == nq {
                90
            } else if nn.ends_with(&nq) {
                70
            } else if !tokens.is_empty() && tokens.iter().all(|t| nn.contains(t.as_str())) {
                60
            } else if is_subsequence(&nq, &nn) {
                20
            } else {
                0
            };
            best = best.max(s);
        }
        // hidden models only on an explicit hit
        if m.hidden && best < 90 { 0 } else { best }
    };
    let mut best: Option<(&ModelInfo, u32)> = None;
    for m in cat {
        let s = score(m);
        if s > 0 && best.is_none_or(|(_, b)| s > b) {
            best = Some((m, s));
        }
    }
    best.map(|(m, _)| m)
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Replace a harness's catalog (adapters call this after discovery). Logs added/removed, drops choices whose
/// model left (one system line each), refreshes the harness note and announces the default after a sign-in.
pub fn set_catalog(k: &Kernel, harness: &str, models: Vec<ModelInfo>, source: &str, error: Option<String>) {
    let (added, removed, vanished, now) = write(k, |s| {
        let rejected = s.rejected.get(harness).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        let models: Vec<ModelInfo> = models
            .into_iter()
            .filter(|m| !rejected.contains(&m.id) && seen.insert(m.id.clone()))
            .map(|mut m| {
                m.harness = harness.to_string();
                m
            })
            .collect();
        let old: Vec<String> = s.catalogs.get(harness).map(|c| c.models.iter().map(|m| m.id.clone()).collect()).unwrap_or_default();
        let new: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
        let added: Vec<String> = new.iter().filter(|i| !old.contains(i)).cloned().collect();
        let removed: Vec<String> = old.iter().filter(|i| !new.contains(i)).cloned().collect();
        let mut vanished = Vec::new();
        if !models.is_empty() {
            let prefs = s.prefs.entry(harness.to_string()).or_default();
            for slot in ["chat", "build", "strong"] {
                let gone = prefs.slot(slot).filter(|c| !new.contains(&c.model)).cloned();
                if let Some(c) = gone {
                    *prefs.slot_mut(slot) = None;
                    vanished.push((slot, c.model));
                }
            }
        }
        let fallback = default_of(&models).map(|m| m.display_name.clone());
        s.catalogs.insert(
            harness.to_string(),
            Catalog { models, source: source.to_string(), fetched_at: crate::state::now_ts(), fetched_unix: (crate::util::now_secs() as u64), error: error.clone() },
        );
        (added, removed, vanished, fallback)
    });
    k.research.log(
        "kernel",
        Kind::ModelsDiscovered,
        json!({ "harness": harness, "source": source, "count": catalog(k, harness).len(), "added": added, "removed": removed, "error": error }),
    );
    for (slot, model) in vanished {
        let what = match slot {
            "strong" => "the strong model",
            "build" => "build mode",
            _ => "chat mode",
        };
        let msg = match &now {
            Some(d) => format!("{model} is no longer offered by your account; {what} uses {d} now."),
            None => format!("{model} is no longer offered by your account; {what} uses the agent's default now."),
        };
        k.system_message("system", &msg);
        k.research.log("kernel", Kind::ModelSwitch, json!({ "harness": harness, "slot": slot, "from": model, "to": now, "reason": "vanished" }));
    }
    note_harness(k, harness);
    let announce = st(k).announce.locked().remove(harness);
    if announce
        && let Some(r) = resolve_for_active(k, harness) {
            k.system_message("system", &format!("Model: {} (from your account). Type /model to change it.", r.label()));
        }
    k.emit_snapshot();
}

/// After a sign-in: the next discovery of this harness announces the default in one system line.
pub fn announce_after_sign_in(k: &Kernel, harness: &str) {
    st(k).announce.locked().insert(harness.to_string());
}

/// The harness card note shows the current model (`… · GPT-X · High`).
fn note_harness(k: &Kernel, harness: &str) {
    let label = resolve_for_active(k, harness).map(|r| r.label());
    k.update_harness(harness, |o| o.model_note = label);
}

/// The provider refused a model for this account (e.g. "model is not supported when using Codex with a ChatGPT
/// account"): drop it from the catalog and from choices. Returns the one system line for the chat.
pub fn reject(k: &Kernel, harness: &str, model: &str, error: &str) -> Option<String> {
    if !is_rejection(error) {
        return None;
    }
    let (had, next) = write(k, |s| {
        let cat = s.catalogs.entry(harness.to_string()).or_default();
        let had = cat.models.iter().any(|m| m.id == model);
        cat.models.retain(|m| m.id != model);
        let next = default_of(&cat.models).map(|m| m.display_name.clone());
        s.rejected.entry(harness.to_string()).or_default().insert(model.to_string());
        if let Some(p) = s.prefs.get_mut(harness) {
            for slot in ["chat", "build", "strong"] {
                if p.slot(slot).is_some_and(|c| c.model == model) {
                    *p.slot_mut(slot) = None;
                }
            }
        }
        (had, next)
    });
    k.research.log("kernel", Kind::ModelSwitch, json!({ "harness": harness, "from": model, "to": next, "reason": "rejected", "error": error.chars().take(300).collect::<String>() }));
    note_harness(k, harness);
    k.emit_snapshot();
    if !had {
        return None;
    }
    Some(match next {
        Some(n) => format!("Your account cannot use {model}. Switched to {n}: send your message again."),
        None => format!("Your account cannot use {model}; no other model was discovered. Type /model refresh."),
    })
}

/// A provider error that says this model is not available to the account.
pub fn is_rejection(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("model")
        && ["not supported", "unsupported", "does not exist", "not found", "do not have access", "does not have access", "not available", "model_not_found", "invalid model"]
            .iter()
            .any(|p| e.contains(p))
}

// ---------- choice ----------

#[derive(Clone, Debug)]
pub struct Resolved {
    pub model: ModelInfo,
    /// In the harness's vocabulary (None = the harness default effort).
    pub effort: Option<String>,
    /// "default" | "strong"
    pub tier: &'static str,
}

impl Resolved {
    /// `GPT-X · Extra high`
    pub fn label(&self) -> String {
        match &self.effort {
            Some(e) => format!("{} · {}", self.model.display_name, effort_label(e)),
            None => self.model.display_name.clone(),
        }
    }
}

fn pick(cat: &[ModelInfo], choice: Option<&Choice>, tier: &'static str) -> Option<Resolved> {
    let chosen = choice.and_then(|c| cat.iter().find(|m| m.id == c.model));
    let model = chosen.or_else(|| default_of(cat))?.clone();
    let want = chosen.and(choice).and_then(|c| c.effort.clone());
    let effort = want
        .and_then(|w| nearest_effort(&w, &model.efforts).or(if model.efforts.is_empty() { Some(w) } else { None }))
        .or_else(|| model.default_effort.clone());
    Some(Resolved { model, effort, tier })
}

/// What a turn of `session` in `mode` runs on (None = the catalog is empty: the agent's own default).
pub fn resolve(k: &Kernel, harness: &str, mode: &str, session: Option<&str>) -> Option<Resolved> {
    let esc = session.and_then(|s| {
        st(k).esc.locked().get(s).filter(|e| e.harness == harness).map(|e| e.choice.clone())
    });
    read(k, |s| {
        let cat = s.catalogs.get(harness).map(|c| c.models.clone()).unwrap_or_default();
        if cat.is_empty() {
            return None;
        }
        let p = s.prefs.get(harness).cloned().unwrap_or_default();
        if let Some(c) = esc {
            return pick(&cat, Some(&c), "strong");
        }
        let c = if mode == "build" { p.build.or(p.chat) } else { p.chat };
        pick(&cat, c.as_ref(), "default")
    })
}

/// Current model of the selected harness for the active session (what the chip shows).
fn resolve_for_active(k: &Kernel, harness: &str) -> Option<Resolved> {
    let (sid, mode) = active(k);
    resolve(k, harness, &mode, Some(&sid))
}

fn active(k: &Kernel) -> (String, String) {
    let g = k.lock();
    let mode = g.sessions.get(&g.active).map(|s| s.rec.mode.clone()).unwrap_or_else(|| "chat".into());
    (g.active.clone(), mode)
}

fn selected_harness(k: &Kernel) -> Option<String> {
    crate::harness::selected(k).map(|s| s.id)
}

/// Set a slot (`current` = the active session's mode). `model` None keeps the slot's model (effort change only).
pub fn set_choice(k: &Kernel, slot: &str, model: Option<&str>, effort: Option<&str>, by: &str) -> Result<Resolved, String> {
    let harness = selected_harness(k).ok_or("No provider is picked yet.")?;
    let (_, mode) = active(k);
    let slot = match slot {
        "" | "current" | "default" => mode.clone(),
        s @ ("chat" | "build" | "strong") => s.to_string(),
        other => return Err(format!("unknown model slot \"{other}\" (current, chat, build, strong)")),
    };
    let cat = catalog(k, &harness);
    if cat.is_empty() {
        return Err("No models were discovered for this provider yet (it uses its own default). Type /model refresh.".into());
    }
    let base = match model {
        Some(q) => find_model(&cat, q).cloned().ok_or_else(|| format!("No model matches \"{q}\". Models: {}", names(&cat)))?,
        None => {
            let tier_mode = if slot == "strong" { "chat" } else { slot.as_str() };
            let cur = if slot == "strong" {
                read(k, |s| s.prefs.get(&harness).and_then(|p| p.strong.clone())).and_then(|c| cat.iter().find(|m| m.id == c.model).cloned())
            } else {
                None
            };
            cur.or_else(|| resolve(k, &harness, tier_mode, None).map(|r| r.model)).ok_or("no model")?
        }
    };
    let effort = match effort {
        Some(e) => {
            let want = parse_effort(e).unwrap_or_else(|| e.to_ascii_lowercase());
            Some(nearest_effort(&want, &base.efforts).ok_or_else(|| format!("{} has no effort \"{e}\"", base.display_name))?)
        }
        None => read(k, |s| s.prefs.get(&harness).and_then(|p| p.slot(&slot).cloned()))
            .filter(|c| c.model == base.id)
            .and_then(|c| c.effort),
    };
    let choice = Choice { model: base.id.clone(), effort: effort.clone() };
    write(k, |s| *s.prefs.entry(harness.clone()).or_default().slot_mut(&slot) = Some(choice.clone()));
    k.research.log(by, Kind::ModelChoice, json!({ "harness": harness, "slot": slot, "model": choice.model, "effort": choice.effort }));
    note_harness(k, &harness);
    k.emit_snapshot();
    let tier = if slot == "strong" { "strong" } else { "default" };
    pick(&cat, Some(&choice), tier).ok_or_else(|| "no model".to_string())
}

fn names(cat: &[ModelInfo]) -> String {
    cat.iter().filter(|m| !m.hidden).map(|m| m.display_name.clone()).collect::<Vec<_>>().join(", ")
}

pub fn clear_choice(k: &Kernel, slot: &str) -> Result<(), String> {
    let harness = selected_harness(k).ok_or("No provider is picked yet.")?;
    let (_, mode) = active(k);
    let slot = if slot.is_empty() || slot == "current" { mode } else { slot.to_string() };
    write(k, |s| *s.prefs.entry(harness.clone()).or_default().slot_mut(&slot) = None);
    k.research.log("user", Kind::ModelChoice, json!({ "harness": harness, "slot": slot, "model": null }));
    note_harness(k, &harness);
    k.emit_snapshot();
    Ok(())
}

pub fn policy(k: &Kernel) -> String {
    read(k, |s| s.policy.clone())
}

pub fn set_policy(k: &Kernel, p: &str) -> Result<(), String> {
    if p != "ask" && p != "auto" {
        return Err(format!("policy must be \"ask\" or \"auto\", got \"{p}\""));
    }
    write(k, |s| s.policy = p.to_string());
    k.research.log("user", Kind::ModelPolicy, json!({ "policy": p }));
    k.emit_snapshot();
    Ok(())
}

// ---------- escalation ----------

/// Strong tier for this session's current turn (and its follow-up). Returns the tool's reply text.
pub async fn request(k: Arc<Kernel>, p: Value) -> Result<Value, String> {
    let tier = arg(&p, "tier").unwrap_or("strong");
    let reason = arg(&p, "reason").unwrap_or("").trim().to_string();
    let spec = crate::harness::selected(&k).ok_or("no provider is running")?;
    let session = k.running_session().unwrap_or_else(|| k.active_session());
    if tier == "default" {
        let had = st(&k).esc.locked().remove(&session).is_some();
        if had {
            k.research.log("agent", Kind::ModelSwitch, json!({ "harness": spec.id, "session": session, "to": "default", "reason": reason, "by": "agent" }));
            note_harness(&k, &spec.id);
            k.emit_snapshot();
        }
        return Ok(json!({ "ok": true, "tier": "default", "text": "Back on the default model." }));
    }
    if tier != "strong" {
        return Err(format!("tier must be \"strong\" or \"default\", got \"{tier}\""));
    }
    if reason.is_empty() {
        return Err("openapp_model_request needs a \"reason\" (one sentence: why this task needs the strong model)".into());
    }
    let strong = read(&k, |s| s.prefs.get(&spec.id).and_then(|p| p.strong.clone()));
    let cat = catalog(&k, &spec.id);
    let Some(strong) = strong.filter(|c| cat.iter().any(|m| m.id == c.model)) else {
        k.research.log("agent", Kind::ModelRequest, json!({ "harness": spec.id, "session": session, "reason": reason, "outcome": "no_strong_model" }));
        return Ok(json!({ "ok": false, "text": "No strong model is set, so the task stays on the current model. Continue; the user can set one with /model strong <name>." }));
    };
    let mode = k.session_rec(&session).map(|r| r.mode).unwrap_or_else(|| "chat".into());
    let from = resolve(&k, &spec.id, &mode, Some(&session));
    let to = pick(&cat, Some(&strong), "strong").ok_or("no model")?;
    if from.as_ref().is_some_and(|f| f.model.id == to.model.id && f.effort == to.effort) {
        return Ok(json!({ "ok": true, "tier": "strong", "text": format!("You already run on {}.", to.label()) }));
    }
    let pol = policy(&k);
    let mut decided_by = "auto";
    if pol != "auto" {
        let keep = from.as_ref().map(|f| f.model.display_name.clone()).unwrap_or_else(|| "the current model".into());
        let choice = card(&k, &session, &format!("Switch to {} for this task?", to.label()), &reason, &keep).await;
        match choice.as_deref() {
            Some("switch") => decided_by = "user",
            Some("always") => {
                decided_by = "user";
                let _ = set_policy(&k, "auto");
            }
            _ => {
                k.research.log("user", Kind::ModelRequest, json!({ "harness": spec.id, "session": session, "reason": reason, "to": to.model.id, "outcome": "declined" }));
                return Ok(json!({ "ok": false, "text": format!("The user keeps {keep}. Continue on it.") }));
            }
        }
    }
    let followup = spec.adapter != crate::harness::Adapter::Pi;
    st(&k).esc.locked().insert(session.clone(), Escalation { harness: spec.id.clone(), choice: strong, reason: reason.clone(), followup });
    k.research.log(
        "agent",
        Kind::ModelSwitch,
        json!({ "harness": spec.id, "session": session, "tier": "strong", "from": from.as_ref().map(|f| f.model.id.clone()), "to": to.model.id, "effort": to.effort, "reason": reason, "policy": pol, "by": decided_by }),
    );
    k.chat_append_to(&session, json!({ "kind": "system", "modelSwitch": true, "text": format!("Switched to {} for this task: {reason}", to.label()) }));
    note_harness(&k, &spec.id);
    k.emit_snapshot();
    if followup {
        Ok(json!({ "ok": true, "tier": "strong", "model": to.model.id, "text": format!("Approved: {} takes over. End this turn now with one short line (no summary); OpenApp continues the task on {} in a follow-up turn.", to.label(), to.model.display_name) }))
    } else {
        if let Err(e) = crate::pi::apply_now(&k, &session).await {
            log::warn!("models: applying the strong model failed: {e}");
        }
        Ok(json!({ "ok": true, "tier": "strong", "model": to.model.id, "text": format!("Approved: the rest of this task runs on {}.", to.label()) }))
    }
}

async fn card(k: &Arc<Kernel>, session: &str, title: &str, reason: &str, keep: &str) -> Option<String> {
    let options = json!([
        { "optionId": "switch", "name": "Switch", "kind": "allow_once" },
        { "optionId": "always", "name": "Always switch", "kind": "allow_always" },
        { "optionId": "keep", "name": format!("Keep {keep}"), "kind": "reject_once" }
    ]);
    let req = crate::consent::ConsentReq::new("model", title, reason).options(options, &["switch", "always"]).session(session).timeout(Duration::from_secs(600)).turn_owned();
    crate::consent::ask(k, req).await.choice
}

/// Codex/ACP: after a turn, the reason of an approved escalation that still needs its follow-up turn.
pub fn take_followup(k: &Kernel, session: &str) -> Option<String> {
    let s = st(k);
    let mut g = s.esc.locked();
    let e = g.get_mut(session).filter(|e| e.followup)?;
    e.followup = false;
    Some(e.reason.clone())
}

/// The session's task is over: revert from the strong model to the default.
pub fn end_turn(k: &Kernel, session: &str) {
    let gone = st(k).esc.locked().remove(session);
    if let Some(e) = gone {
        k.research.log("kernel", Kind::ModelSwitch, json!({ "harness": e.harness, "session": session, "tier": "default", "from": e.choice.model, "reason": "turn ended" }));
        note_harness(k, &e.harness);
        k.emit_snapshot();
    }
}

// ---------- discovery ----------

/// Discover the selected harness's models now (one discovery per harness at a time).
pub async fn refresh(k: &Arc<Kernel>) -> Result<Value, String> {
    let spec = crate::harness::selected(k).ok_or("No provider is picked yet.")?;
    if !st(k).refreshing.locked().insert(spec.id.clone()) {
        return Ok(json!({ "harness": spec.id, "running": true }));
    }
    let r = match spec.adapter {
        crate::harness::Adapter::Codex => crate::codex::discover_models(k).await,
        crate::harness::Adapter::Pi => crate::pi::discover_models(k).await,
        crate::harness::Adapter::Acp => Ok(catalog(k, &spec.id).len()),
    };
    st(k).refreshing.locked().remove(&spec.id);
    let n = r?;
    Ok(json!({ "harness": spec.id, "count": n, "models": catalog(k, &spec.id) }))
}

/// Discovery in the background (after a start or a sign-in).
pub fn refresh_soon(k: &Arc<Kernel>) {
    let k = k.clone();
    tokio::spawn(async move {
        if let Err(e) = refresh(&k).await {
            log::warn!("models: discovery failed: {e}");
        }
    });
}

fn daily(k: &Kernel) {
    let Some(k) = k.arc() else { return };
    k.clone().spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1800)).await;
            if k.is_shutdown() {
                return;
            }
            let Some(h) = selected_harness(&k) else { continue };
            let stale = read(&k, |s| s.catalogs.get(&h).is_none_or(|c| (crate::util::now_secs() as u64).saturating_sub(c.fetched_unix) > DAY_SECS));
            if stale && k.agent_status().0 == "ready"
                && let Err(e) = refresh(&k).await {
                    log::warn!("models: daily discovery failed: {e}");
                }
        }
    });
}

// ---------- snapshot / describe ----------

/// Snapshot `models`: every cached catalog (the shell shows the selected harness's).
pub fn snapshot_models(k: &Kernel) -> Vec<Value> {
    read(k, |s| s.catalogs.values().flat_map(|c| c.models.iter().map(|m| json!(m))).collect())
}

/// Snapshot `modelPrefs` for the selected harness; `mode`/`session` = the active session.
pub fn snapshot_prefs(k: &Kernel, harness: Option<&str>, mode: &str, session: &str) -> Value {
    let Some(h) = harness else { return Value::Null };
    let escalated = st(k).esc.locked().get(session).filter(|e| e.harness == h).map(|e| e.reason.clone());
    let current = resolve(k, h, mode, Some(session));
    read(k, |s| {
        let p = s.prefs.get(h).cloned().unwrap_or_default();
        let c = s.catalogs.get(h);
        json!({
            "harness": h,
            "chat": p.chat, "build": p.build, "strong": p.strong,
            "policy": s.policy,
            "source": c.map(|c| c.source.clone()),
            "fetchedAt": c.map(|c| c.fetched_at.clone()),
            "error": c.and_then(|c| c.error.clone()),
            "escalated": escalated,
            "current": current.as_ref().map(|r| json!({ "model": r.model.id, "effort": r.effort, "tier": r.tier, "label": r.label() })),
        })
    })
}

/// `agent.model` / `agent.effort` / `agent.modelName` for the snapshot.
pub fn agent_fields(k: &Kernel, harness: Option<&str>, mode: &str, session: &str) -> Option<(String, Option<String>, String, &'static str)> {
    let r = resolve(k, harness?, mode, Some(session))?;
    Some((r.model.id.clone(), r.effort.clone(), r.model.display_name.clone(), r.tier))
}

fn describe_doc(k: &Kernel) -> Value {
    let h = selected_harness(k);
    let (sid, mode) = active(k);
    json!({
        "harness": h,
        "catalog": h.as_deref().map(|h| catalog(k, h)).unwrap_or_default(),
        "prefs": snapshot_prefs(k, h.as_deref(), &mode, &sid),
        "escalate": "openapp_model_request { tier: \"strong\", reason } when a task is hard (policy ask = the user confirms; auto = switches). The task then runs on the user's strong model and reverts after it.",
        "chat": "/model [chat|build|strong] <name> [effort] · /model policy ask|auto · /model refresh · /model default",
    })
}

// ---------- chat command ----------

fn slash_model(k: &Arc<Kernel>, rest: &str) {
    k.chat_append(json!({ "kind": "user", "text": format!("/model {rest}").trim().to_string() }));
    let reply = |m: String| k.system_message("system", &m);
    let words: Vec<&str> = rest.split_whitespace().collect();
    let Some(harness) = selected_harness(k) else {
        return reply("No provider is picked yet.".into());
    };
    let cat = catalog(k, &harness);
    match words.as_slice() {
        [] => {
            let cur = resolve_for_active(k, &harness);
            let strong = read(k, |s| s.prefs.get(&harness).and_then(|p| p.strong.clone()));
            let list: Vec<String> = cat
                .iter()
                .filter(|m| !m.hidden)
                .map(|m| format!("{}{}", m.display_name, if m.efforts.is_empty() { String::new() } else { format!(" ({})", m.efforts.iter().map(|e| effort_label(e)).collect::<Vec<_>>().join(", ")) }))
                .collect();
            let mut msg = match cur {
                Some(c) => format!("Model: {}.", c.label()),
                None => "Model: the agent's own default (none discovered).".into(),
            };
            if let Some(s) = strong {
                msg.push_str(&format!(" Strong: {}.", display(&cat, &s.model)));
            }
            msg.push_str(&format!(" Escalation: {}.", policy(k)));
            if !list.is_empty() {
                msg.push_str(&format!(" Available: {}.", list.join("; ")));
            }
            msg.push_str(" Use /model <name> [effort], /model strong <name>, /model policy ask|auto.");
            reply(msg)
        }
        ["refresh"] | ["refresh", "hidden"] => {
            if words.len() == 2 {
                write(k, |s| s.include_hidden = true);
            }
            let k2 = k.clone();
            tokio::spawn(async move {
                match refresh(&k2).await {
                    Ok(v) => k2.system_message("system", &format!("Models refreshed: {} found.", v["count"].as_u64().unwrap_or(0))),
                    Err(e) => k2.system_message("error", &format!("Model discovery failed: {e}")),
                }
            });
        }
        ["policy"] => reply(format!("Escalation policy: {}. Use /model policy ask|auto.", policy(k))),
        ["policy", p] => match set_policy(k, p) {
            Ok(()) => reply(if *p == "auto" { "The agent switches to the strong model on its own when a task is hard.".into() } else { "The agent asks before it switches to the strong model.".into() }),
            Err(e) => reply(e),
        },
        ["default"] | ["reset"] => match clear_choice(k, "current") {
            Ok(()) => reply(format!("Model: {} (the provider's default).", resolve_for_active(k, &harness).map(|r| r.label()).unwrap_or_else(|| "agent default".into()))),
            Err(e) => reply(e),
        },
        _ => {
            let (slot, words) = match words[0] {
                s @ ("chat" | "build" | "strong") => (s, &words[1..]),
                _ => ("current", &words[..]),
            };
            // trailing effort: one word ("xhigh") or two ("extra high")
            let (query, effort) = split_effort(words);
            let model = if query.is_empty() { None } else { Some(query.as_str()) };
            if model.is_none() && effort.is_none() {
                return reply("Name a model, e.g. /model strong <name>.".into());
            }
            match set_choice(k, slot, model, effort.as_deref(), "user") {
                Ok(r) => reply(match slot {
                    "strong" => format!("Strong model: {}.", r.label()),
                    "build" => format!("Build mode model: {}.", r.label()),
                    "chat" => format!("Chat mode model: {}.", r.label()),
                    _ => format!("Model: {}.", r.label()),
                }),
                Err(e) => reply(e),
            }
        }
    }
}

/// Split `["sol", "extra", "high"]` into ("sol", Some("xhigh")).
fn split_effort(words: &[&str]) -> (String, Option<String>) {
    if words.len() >= 2 {
        let two = format!("{} {}", words[words.len() - 2], words[words.len() - 1]);
        if (words[words.len() - 2].eq_ignore_ascii_case("extra") || words[words.len() - 2].eq_ignore_ascii_case("very"))
            && let Some(e) = parse_effort(&two) {
                return (words[..words.len() - 2].join(" "), Some(e));
            }
    }
    if let Some(last) = words.last()
        && let Some(e) = parse_effort(last) {
            return (words[..words.len() - 1].join(" "), Some(e));
        }
    (words.join(" "), None)
}

// ---------- primitive ----------

fn on_event(k: &Kernel, ev: &Event) {
    if let Event::Started = ev {
        daily(k);
    }
}

pub fn primitive() -> Primitive {
    Primitive {
        name: "models",
        purpose: "Models the signed-in account offers, discovered at runtime per provider (never hardcoded); the user's choice per mode, a strong tier and agent-requested escalation.",
        example: "chat: /model <name> xhigh · /model strong <name> · /model policy auto · agent: openapp_model_request { tier: \"strong\", reason: \"multi-module refactor\" }",
        class: Class::Free,
        actions: vec![
            Action {
                name: "model.set",
                args: "{ model?, effort?, slot?: \"current\"|\"chat\"|\"build\"|\"strong\" } — choose a discovered model (fuzzy name) and effort",
                handler: handler(|k, p, c| async move {
                    let by = if c.id == "agent" { "agent" } else { "user" };
                    let r = set_choice(&k, arg(&p, "slot").unwrap_or("current"), arg(&p, "model"), arg(&p, "effort"), by)?;
                    Ok::<_, String>(json!({ "model": r.model.id, "effort": r.effort, "label": r.label() }))
                }),
            },
            Action {
                name: "model.clear",
                args: "{ slot?: \"current\"|\"chat\"|\"build\"|\"strong\" } — back to the provider's default",
                handler: handler(|k, p, _c| async move {
                    clear_choice(&k, arg(&p, "slot").unwrap_or("current"))?;
                    Ok::<_, String>(json!({ "ok": true }))
                }),
            },
            Action {
                name: "model.policy",
                args: "{ policy: \"ask\"|\"auto\" } — escalation to the strong model: ask the user, or switch",
                handler: handler(|k, p, _c| async move {
                    set_policy(&k, arg(&p, "policy").unwrap_or(""))?;
                    Ok::<_, String>(json!({ "policy": policy(&k) }))
                }),
            },
            Action {
                name: "models.refresh",
                args: "{ includeHidden? } — discover the provider's models again",
                handler: handler(|k, p, _c| async move {
                    if let Some(h) = p.get("includeHidden").and_then(Value::as_bool) {
                        write(&k, |s| s.include_hidden = h);
                    }
                    refresh(&k).await
                }),
            },
        ],
        tools: vec![McpTool {
            name: "openapp_model_request",
            description: "Ask to run the current task on the user's strong model (tier \"strong\") when it is hard: a large refactor, a tricky bug, an architecture decision. Give a one-sentence reason. The user's policy decides (ask = they confirm in a card; auto = it switches). After the task the kernel reverts to the default model. tier \"default\" switches back early.",
            schema: json!({
                "type": "object",
                "properties": {
                    "tier": { "type": "string", "enum": ["strong", "default"] },
                    "reason": { "type": "string", "description": "One sentence: why this task needs the strong model" }
                },
                "required": ["tier", "reason"]
            }),
            handler: handler(|k, p, _c| async move { request(k, p).await }),
        }],
        data: vec![DataSource {
            name: "models",
            doc: "list items [{ id, title: displayName, subtitle: efforts · description, icon: \"cpu\", active, action: {host:\"model.set\", model} }] — the selected provider's discovered models",
            get: data_fn(|k| {
                let Some(h) = selected_harness(k) else { return json!([]) };
                let cur = resolve_for_active(k, &h).map(|r| r.model.id);
                Value::Array(
                    catalog(k, &h)
                        .into_iter()
                        .filter(|m| !m.hidden)
                        .map(|m| {
                            let efforts = m.efforts.iter().map(|e| effort_label(e)).collect::<Vec<_>>().join(", ");
                            let subtitle = [efforts, m.description.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                            json!({ "id": m.id, "title": m.display_name, "subtitle": subtitle, "icon": "cpu", "active": cur.as_deref() == Some(m.id.as_str()), "action": { "host": "model.set", "model": m.id } })
                        })
                        .collect(),
                )
            }),
        }],
        slash: vec![("/model", Arc::new(slash_model))],
        on_event: Some(Arc::new(on_event)),
        describe_keys: vec![("models", data_fn(describe_doc))],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, name: &str, efforts: &[&str], default: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: name.into(),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            default_effort: efforts.get(1).map(|e| e.to_string()),
            is_default: default,
            harness: "h".into(),
            ..Default::default()
        }
    }

    #[test]
    fn efforts_map_between_vocabularies() {
        let codex: Vec<String> = ["low", "medium", "high", "xhigh", "max", "ultra"].iter().map(|s| s.to_string()).collect();
        let pi: Vec<String> = ["off", "low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect();
        assert_eq!(nearest_effort("ultra", &pi).as_deref(), Some("max"));
        assert_eq!(nearest_effort("none", &pi).as_deref(), Some("off"));
        assert_eq!(nearest_effort("off", &codex).as_deref(), Some("low"));
        assert_eq!(nearest_effort("minimal", &codex).as_deref(), Some("low"));
        assert_eq!(nearest_effort("XHigh", &codex).as_deref(), Some("xhigh"));
        assert_eq!(parse_effort("Extra high").as_deref(), Some("xhigh"));
        assert_eq!(parse_effort("x-high").as_deref(), Some("xhigh"));
        assert_eq!(effort_label("xhigh"), "Extra high");
        assert_eq!(effort_label("high"), "High");
        assert_eq!(split_effort(&["fake", "two", "extra", "high"]), ("fake two".to_string(), Some("xhigh".to_string())));
        assert_eq!(split_effort(&["fake", "max"]), ("fake".to_string(), Some("max".to_string())));
        assert_eq!(split_effort(&["fake"]), ("fake".to_string(), None));
    }

    #[test]
    fn fuzzy_match_and_defaults() {
        let cat = vec![
            m("zeta-9-pro", "Zeta-9 Pro", &["low", "medium"], false),
            m("zeta-9-mini", "Zeta-9 Mini", &["low", "medium"], true),
            m("omega/zeta-8-mini", "Zeta-8 Mini", &["off", "high"], false),
        ];
        assert_eq!(find_model(&cat, "mini").map(|m| m.id.as_str()), Some("zeta-9-mini"));
        assert_eq!(find_model(&cat, "Zeta-8 Mini").map(|m| m.id.as_str()), Some("omega/zeta-8-mini"));
        assert_eq!(find_model(&cat, "z8mini").map(|m| m.id.as_str()), Some("omega/zeta-8-mini"));
        assert_eq!(find_model(&cat, "zeta-8-mini").map(|m| m.id.as_str()), Some("omega/zeta-8-mini"));
        assert_eq!(find_model(&cat, "pro").map(|m| m.id.as_str()), Some("zeta-9-pro"));
        assert!(find_model(&cat, "qqq").is_none());
        assert_eq!(default_of(&cat).map(|m| m.id.as_str()), Some("zeta-9-mini"));
        let r = pick(&cat, Some(&Choice { model: "omega/zeta-8-mini".into(), effort: Some("xhigh".into()) }), "default").unwrap();
        assert_eq!((r.model.id.as_str(), r.effort.as_deref()), ("omega/zeta-8-mini", Some("high")));
        let r = pick(&cat, Some(&Choice { model: "gone".into(), effort: Some("max".into()) }), "default").unwrap();
        assert_eq!((r.model.id.as_str(), r.effort.as_deref()), ("zeta-9-mini", Some("medium")));
        assert!(is_rejection("OpenAI API error (400): The 'x' model is not supported when using Codex with a ChatGPT account."));
        assert!(!is_rejection("rate limit exceeded"));
    }
}
