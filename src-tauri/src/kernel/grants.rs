//! The user's remembered decisions: one store, `<state>/grants.json` = `{ "<kind>": { "<target>": grant } }`.
//!
//! Every grant has one shape, `{ allowed, scope, ts, ...extra }` (extra: `actor`, `source_hash`). Older files with
//! bare booleans (`"microphone": { "<module>": true }`) are read as `{ allowed, scope: "workspace" }` and written
//! back in the new shape. Writes are atomic (temp file + rename) and every read-modify-write runs under one lock,
//! so two decisions made at the same time are never lost.
//!
//! Kinds: `network` (host), `web.control` (host), `lan` (`<module>:<port>`), `microphone`, `camera`,
//! `clipboard.read`, `shortcut.global`, `background`, `login_item` (module), `host_modules` (module).
//! Folder grants (`folder:<name>`) are security-scoped bookmarks in `<state>/folders.json` (primitives/files.rs).

use crate::research::Kind;
use super::*;
use std::path::Path;

pub struct GrantStore {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Kinds listed in host data `grants`, in display order (folder grants follow).
const LISTED: &[&str] = &["network", "web.control", "lan", "microphone", "clipboard.read", "shortcut.global", "camera"];

fn migrate(g: Value) -> Value {
    match g {
        Value::Bool(b) => json!({ "allowed": b, "scope": "workspace" }),
        other => other,
    }
}

impl GrantStore {
    pub fn new(state_dir: &Path) -> Self {
        Self { path: state_dir.join("grants.json"), lock: Mutex::new(()) }
    }

    fn read_unlocked(&self) -> Map<String, Value> {
        let mut m = std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for kind in m.values_mut() {
            if let Some(o) = kind.as_object_mut() {
                for g in o.values_mut() {
                    *g = migrate(g.take());
                }
            }
        }
        m
    }

    /// Read-modify-write under the store lock; the file is replaced atomically when `f` changed it.
    pub fn update<R>(&self, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
        let _g = self.lock.locked();
        let before = self.read_unlocked();
        let mut m = before.clone();
        let r = f(&mut m);
        if m != before
            && let Err(e) = crate::state::write_atomic(&self.path, &serde_json::to_vec_pretty(&m).unwrap_or_default())
        {
            log::warn!("grants: write failed: {e}");
        }
        r
    }

    /// The grant object of `kind` / `target`.
    pub fn get(&self, kind: &str, target: &str) -> Option<Value> {
        let _g = self.lock.locked();
        self.read_unlocked().get(kind)?.get(target).cloned()
    }

    pub fn allowed(&self, kind: &str, target: &str) -> Option<bool> {
        self.get(kind, target)?.get("allowed")?.as_bool()
    }

    /// Store a decision: `{ allowed, scope: "workspace", ts: now, ...extra }`.
    pub fn set(&self, kind: &str, target: &str, allowed: bool, extra: Value) {
        let mut g = json!({ "allowed": allowed, "scope": "workspace", "ts": now_ts() });
        if let (Some(o), Value::Object(x)) = (g.as_object_mut(), extra) {
            o.extend(x);
        }
        self.update(|m| {
            let k = m.entry(kind.to_string()).or_insert_with(|| json!({}));
            if !k.is_object() {
                *k = json!({});
            }
            k[target] = g;
        });
    }

    pub fn remove(&self, kind: &str, target: &str) -> bool {
        self.update(|m| m.get_mut(kind).and_then(Value::as_object_mut).and_then(|o| o.remove(target)).is_some())
    }

    /// All grants of a kind: (target, grant).
    pub fn of_kind(&self, kind: &str) -> Vec<(String, Value)> {
        let _g = self.lock.locked();
        self.read_unlocked().get(kind).and_then(Value::as_object).map(|o| o.iter().map(|(t, g)| (t.clone(), g.clone())).collect()).unwrap_or_default()
    }
}

impl Kernel {
    /// Host data `grants`: `[{ id: "<kind>:<target>", kind, target, allowed, scope, ts }]` (+ folder grants).
    pub fn grants_list(&self) -> Vec<Value> {
        let all = {
            let _g = self.grants.lock.locked();
            self.grants.read_unlocked()
        };
        let mut out = Vec::new();
        for kind in LISTED {
            for (target, g) in all.get(*kind).and_then(Value::as_object).into_iter().flatten() {
                out.push(json!({
                    "id": format!("{kind}:{target}"),
                    "kind": kind,
                    "target": target,
                    "allowed": g["allowed"].as_bool().unwrap_or(false),
                    "scope": g.get("scope").cloned().unwrap_or_else(|| json!("workspace")),
                    "ts": g["ts"],
                }));
            }
        }
        out.extend(crate::primitives::files::grants_list(self));
        out
    }

    /// Host action `grant.revoke {id}`.
    pub fn grant_revoke(&self, id: &str) -> Result<(), String> {
        // §21 G5 folder grants live in <state>/folders.json (revoking restarts the VM without the mount)
        if let Some(name) = id.strip_prefix("folder:")
            && let Some(k) = self.arc()
        {
            let name = name.to_string();
            self.spawn(async move {
                if let Err(e) = crate::primitives::files::revoke_name(&k, &name, "user").await {
                    k.notify(&e);
                }
            });
            return Ok(());
        }
        let (kind, target) = id.split_once(':').ok_or_else(|| format!("grant id must be \"network:<host>\", \"web.control:<host>\" or \"microphone:<module>\", got \"{id}\""))?;
        if !self.grants.remove(kind, target) {
            return Err(format!("no grant \"{id}\""));
        }
        self.research.log("user", Kind::GrantRevoke, json!({ "id": id }));
        self.emit_snapshot();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oa-grants-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let d = tmp("race");
        let store = Arc::new(GrantStore::new(&d));
        let n = 32;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let s = store.clone();
                std::thread::spawn(move || {
                    let kind = if i % 2 == 0 { "network" } else { "microphone" };
                    s.set(kind, &format!("t{i}"), i % 3 != 0, json!({ "actor": "agent" }));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for i in 0..n {
            let kind = if i % 2 == 0 { "network" } else { "microphone" };
            assert_eq!(store.allowed(kind, &format!("t{i}")), Some(i % 3 != 0), "grant {kind}:t{i} lost");
        }
        assert_eq!(store.of_kind("network").len() + store.of_kind("microphone").len(), n);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_shapes_migrate_on_read() {
        let d = tmp("migrate");
        std::fs::write(d.join("grants.json"), r#"{ "microphone": { "rec": true, "other": false }, "network": { "a.example": { "allowed": true, "scope": "workspace", "ts": "x", "actor": "agent" } } }"#).unwrap();
        let s = GrantStore::new(&d);
        assert_eq!(s.allowed("microphone", "rec"), Some(true));
        assert_eq!(s.allowed("microphone", "other"), Some(false));
        assert_eq!(s.get("network", "a.example").unwrap()["actor"], "agent");
        s.set("camera", "m", true, Value::Null);
        let raw: Value = serde_json::from_slice(&std::fs::read(d.join("grants.json")).unwrap()).unwrap();
        assert_eq!(raw["microphone"]["rec"], json!({ "allowed": true, "scope": "workspace" }));
        assert_eq!(raw["camera"]["m"]["scope"], "workspace");
        assert!(s.remove("microphone", "rec") && !s.remove("microphone", "rec"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
