//! §21 G4 notifications with actions: `notify { title, body, actions?: [{id, label, action?}], action? }`
//! (the built-in `notify { text }` stays a toast). Posted through UserNotifications by the bundled
//! `openapp-macos` helper (`OpenAppNotify.app`); answers come back as `notify.response` and run the module's
//! action (`action` for a click, the button's own `action` or `action` + `{ actionId }` for a button).
//! Without the helper: an AppleScript notification without buttons.

use crate::research::Kind;
use crate::util::MutexExt;
use super::shell::{self, s, st};
use super::*;
use crate::host_services::Sidecar;
use std::path::PathBuf;

#[derive(Default)]
struct NotifyState {
    helper: OnceLock<Option<Arc<Sidecar>>>,
}

pub fn primitive() -> Primitive {
    let post = handler(|k, p, c| async move { post(&k, &p, &c).await });
    Primitive {
        name: "notify",
        purpose: "macOS notifications with buttons; a click or a button runs the module's action",
        example: r#"{ "host": "notify", "title": "Render done", "body": "mix.wav is ready", "actions": [{ "id": "open", "label": "Open" }], "action": { "tool": "on_notification" } }"#,
        class: Class::Free,
        actions: vec![Action { name: "notify.post", args: "{ title, body?, subtitle?, actions?: [{ id, label, action? }], action?, sound? } (also as host \"notify\" with a title)", handler: post.clone() }],
        endpoints: vec![
            Endpoint { method: "notify.post", args: "{ title, body?, actions?, action? }", handler: post.clone() },
            Endpoint { method: "notify", args: "{ title, body?, actions?, action? } (same as notify.post)", handler: post },
        ],
        ..Default::default()
    }
}

/// `Contents/Helpers/OpenAppNotify.app` in the app bundle; debug builds also take `OPENAPP_NOTIFY_HELPER` and
/// the one build.rs built (see `bundle.rs`).
pub fn helper_bin() -> Option<PathBuf> {
    crate::bundle::notify_helper()
}

fn helper(k: &Arc<Kernel>) -> Option<Arc<Sidecar>> {
    k.prims
        .get::<NotifyState>()
        .helper
        .get_or_init(|| {
            let bin = helper_bin()?;
            let sc = Sidecar::new("openapp-notify", bin, k.workspace.join(".openapp").join("logs").join("_host.log"), Vec::new(), None);
            let weak = Arc::downgrade(k);
            sc.set_on_notify(Arc::new(move |method, params| {
                if method == "notify.response"
                    && let Some(k) = weak.upgrade()
                {
                    let k2 = k.clone();
                    k.spawn(async move { on_response(&k2, params).await });
                }
            }));
            Some(sc)
        })
        .clone()
}

/// Built-in host action `notify` with a `title` (or `body`) lands here.
pub fn is_system(p: &Value) -> bool {
    p.get("title").is_some_and(Value::is_string) || p.get("body").is_some_and(Value::is_string)
}

async fn post(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p).ok();
    let title = s(p, "title").or_else(|| s(p, "text")).ok_or("notify needs \"title\"")?.to_string();
    let body = s(p, "body").unwrap_or("").to_string();
    let mut actions = Vec::new();
    for a in p.get("actions").and_then(Value::as_array).into_iter().flatten().take(4) {
        let (Some(id), Some(label)) = (s(a, "id"), s(a, "label")) else {
            return Err("each of \"actions\" needs { id, label }".into());
        };
        actions.push(json!({ "id": id, "label": label }));
    }
    if (p.get("action").is_some() || p.get("actions").is_some()) && module.is_none() {
        return Err("notification actions need \"module\" (the module whose action runs)".into());
    }
    let id = shell::next_id(k, "n_");
    st(k).notes.locked().insert(id.clone(), json!({ "module": module, "action": p.get("action"), "actions": p.get("actions") }));
    k.research.log(&caller.id, Kind::Notify, json!({ "module": module, "actions": actions.len() }));
    let params = json!({ "id": id, "title": title, "body": body, "subtitle": p.get("subtitle"), "sound": p.get("sound").and_then(Value::as_bool).unwrap_or(true), "actions": actions });
    let mut why = "buttons and click actions need the notification helper (OpenAppNotify.app)".to_string();
    if let Some(sc) = helper(k) {
        match sc.call("notify.post", params, std::time::Duration::from_secs(20)).await {
            Ok(v) => return Ok(json!({ "ok": true, "id": id, "via": "UserNotifications", "actions": v["actions"] })),
            Err(e) => {
                log::warn!("notify helper: {e}; falling back to AppleScript");
                why = format!("posted without buttons: {e}");
            }
        }
    }
    let script = format!("display notification {} with title {}", applescript_str(&body), applescript_str(&title));
    let out = tokio::process::Command::new("osascript").args(["-e", &script]).output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("notification failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(json!({ "ok": true, "id": id, "via": "AppleScript", "note": why }))
}

fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The user answered a notification (helper → `notify.response { id, action }`).
pub async fn on_response(k: &Arc<Kernel>, params: Value) {
    let id = params["id"].as_str().unwrap_or("").to_string();
    let action_id = params["action"].as_str().unwrap_or("default").to_string();
    let Some(note) = st(k).notes.locked().remove(&id) else { return };
    k.research.log("user", Kind::NotifyResponse, json!({ "module": note["module"], "action": action_id }));
    if action_id == "dismiss" {
        return;
    }
    if action_id == "default" {
        let _ = super::shell_misc::show_main(k);
    }
    let Some(module) = note["module"].as_str().map(String::from) else { return };
    let button = note["actions"].as_array().and_then(|a| a.iter().find(|b| b["id"] == action_id.as_str()).cloned());
    let action = button.as_ref().and_then(|b| b.get("action").filter(|a| a.is_object()).cloned()).or_else(|| note.get("action").filter(|a| a.is_object()).cloned());
    if let Some(a) = action {
        let _ = shell::run_action(k, &format!("notify:{id}"), &module, &a, json!({ "notification": id, "actionId": action_id })).await;
    }
}

/// For tests: answer a notification as if the user clicked (`action` = button id or "default").
pub async fn simulate_response(k: &Arc<Kernel>, id: &str, action: &str) {
    on_response(k, json!({ "id": id, "action": action })).await;
}
