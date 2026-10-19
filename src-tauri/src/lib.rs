#![recursion_limit = "256"]
//! OpenApp kernel. The core (`kernel`, `mcp_server`, `acp`, ...) has no Tauri types;
//! this file wires it to Tauri: commands, events and the `oamod://` protocol.

use crate::research::Kind;
use crate::util::MutexExt;

pub mod acp;
pub mod bundle;
pub mod codex;
pub mod consent;
pub mod egress;
pub mod harness;
pub mod host_modules;
pub mod host_services;
pub mod kernel;
pub mod layout;
pub mod mcp_server;
pub mod modules;
pub mod net;
pub mod oauth;
pub mod paths;
pub mod pi;
pub mod primitives;
pub mod protocol;
pub mod research;
pub mod rpc;
pub mod secrets;
pub mod state;
pub mod theme;
pub mod util;
pub mod views;
pub mod vm;
pub mod web;
pub mod web_control;
pub mod workspace;

use kernel::{EventSink, Kernel};
use serde_json::{Value, json};
use std::sync::Arc;
use tauri::{Emitter, Manager};

struct TauriSink(tauri::AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: &str, payload: Value) {
        if let Err(e) = self.0.emit(event, payload) {
            log::warn!("emit {event} failed: {e}");
        }
    }
}

type K<'a> = tauri::State<'a, Arc<Kernel>>;

#[tauri::command]
async fn get_snapshot(k: K<'_>) -> Result<state::Snapshot, String> {
    Ok(k.snapshot())
}

#[tauri::command]
async fn send_prompt(k: K<'_>, text: String) -> Result<(), String> {
    k.inner().send_prompt(text);
    Ok(())
}

#[tauri::command]
async fn cancel_turn(k: K<'_>) -> Result<(), String> {
    k.cancel_turn();
    Ok(())
}

#[tauri::command]
async fn respond_permission(k: K<'_>, request_id: String, option_id: Option<String>) -> Result<(), String> {
    k.respond_permission(&request_id, option_id)
}

#[tauri::command]
async fn ui_action(k: K<'_>, placement_id: String, action: Value, input: Option<Value>) -> Result<Value, String> {
    Ok(match k.inner().ui_action_value(&placement_id, action, input).await {
        Ok(Value::Null) => json!({ "ok": true }),
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    })
}

#[tauri::command]
async fn iframe_call(k: K<'_>, module: String, tool: String, args: Option<Value>) -> Result<Value, String> {
    k.inner().iframe_call(&module, &tool, args.unwrap_or(Value::Null)).await
}

#[tauri::command]
async fn web_bounds(k: K<'_>, placement_id: String, rect: Option<kernel::WebRect>) -> Result<(), String> {
    k.web_bounds(&placement_id, rect)
}

#[tauri::command]
async fn answer_question(k: K<'_>, request_id: String, answers: Option<serde_json::Map<String, Value>>) -> Result<(), String> {
    k.answer_question(&request_id, answers)
}

#[tauri::command]
async fn save_audio(
    k: K<'_>,
    placement_id: String,
    data: Value,
    mime: String,
    action: Option<Value>,
    duration_ms: Option<u64>,
) -> Result<Value, String> {
    primitives::audio::save(k.inner(), &placement_id, data, &mime, action, duration_ms).await
}

#[tauri::command]
async fn select_harness(k: K<'_>, id: String) -> Result<(), String> {
    harness::select(k.inner(), &id)
}

#[tauri::command]
async fn harness_login(k: K<'_>, id: String, method: Option<String>) -> Result<(), String> {
    harness::login(k.inner(), &id, method.as_deref())
}

/// §17: input for an open sign-in (Pi: pasted redirect URL, or an API key written into the VM). Never logged.
#[tauri::command]
async fn harness_login_submit(k: K<'_>, id: String, kind: String, value: String, provider: Option<String>) -> Result<(), String> {
    harness::login_submit(k.inner(), &id, &kind, &value, provider.as_deref()).await
}

/// §17: remove one stored login of a VM harness (Pi: a provider in the VM's auth.json) and restart it.
#[tauri::command]
async fn harness_logout(k: K<'_>, id: String, provider: String) -> Result<(), String> {
    harness::logout(k.inner(), &id, &provider).await
}

/// §20.1: the value goes straight to the Keychain (never into chat, logs or the research log).
#[tauri::command]
async fn submit_secret(k: K<'_>, request_id: String, value: Option<String>) -> Result<(), String> {
    k.submit_secret(&request_id, value)
}

/// §21 G1: subscribe a view to a live channel → `{ sub, replay }`; messages arrive as `oa://channel`.
#[tauri::command]
async fn channel_subscribe(k: K<'_>, placement_id: String, topic: String) -> Result<Value, String> {
    primitives::channels::view_subscribe(k.inner(), &placement_id, &topic)
}

#[tauri::command]
async fn channel_unsubscribe(k: K<'_>, sub: u64) -> Result<(), String> {
    primitives::channels::view_unsubscribe(k.inner(), sub);
    Ok(())
}

/// `{ topic, data | b64 }` as the placement's module.
#[tauri::command]
async fn channel_publish(k: K<'_>, placement_id: String, message: Value) -> Result<Value, String> {
    primitives::channels::view_publish(k.inner(), &placement_id, &message)
}

/// §21 registry Tauri commands (`commands` of a primitive).
#[tauri::command]
async fn primitive_command(k: K<'_>, name: String, args: Option<Value>, placement_id: Option<String>) -> Result<Value, String> {
    let h = primitives::command(&name).ok_or_else(|| format!("unknown primitive command \"{name}\""))?;
    let caller = match &placement_id {
        Some(p) => primitives::Caller::for_placement(k.inner(), p),
        None => primitives::Caller::new("user"),
    };
    h(k.inner().clone(), args.unwrap_or(Value::Null), caller).await.map_err(String::from)
}

#[tauri::command]
async fn report_ui_error(k: K<'_>, placement_id: String, message: String) -> Result<(), String> {
    k.report_ui_error(&placement_id, &message);
    Ok(())
}

/// Guest image download / load progress → channel `kernel/vm-image`, the VM harness cards' note
/// (snapshot), and the research log (phase changes only).
fn watch_vm_image(k: &Arc<Kernel>) {
    let weak = Arc::downgrade(k);
    let last_phase = std::sync::Mutex::new(String::new());
    vm::on_image_progress(Arc::new(move |p: &Value| {
        let Some(k) = weak.upgrade() else { return };
        primitives::channels::publish(&k, "kernel/vm-image", primitives::channels::ChannelData::Json(p.clone()));
        let phase = p["phase"].as_str().unwrap_or("").to_string();
        let changed = {
            let mut l = last_phase.locked();
            std::mem::replace(&mut *l, phase.clone()) != phase
        };
        // phase changes, retries and errors (not every progress tick)
        if changed || p.get("error").is_some() {
            let mut data = p.clone();
            if let Some(o) = data.as_object_mut() {
                o.remove("note");
            }
            k.research.log("kernel", Kind::VmImage, data);
        }
        let vm_ids: Vec<String> = harness::specs(None).into_iter().filter(|s| s.vm && harness::is_allowed(&s.id)).map(|s| s.id).collect();
        let note = p["note"].as_str().map(String::from);
        if phase == "ready" {
            // back to the detection note (the card list may still show the progress text)
            let ready = "Runs in an isolated Linux VM with its own sign-in".to_string();
            for h in k.harnesses.locked().iter_mut().filter(|h| h.vm && !h.coming_soon) {
                h.note = Some(ready.clone());
            }
            for id in &vm_ids {
                k.update_harness(id, |o| o.note = None);
            }
        } else if changed || note.is_some() {
            for id in &vm_ids {
                let n = note.clone();
                k.update_harness(id, |o| o.note = n);
            }
        }
    }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // a release build runs only from a complete OpenApp.app (no build-machine paths)
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    if let Err(e) = bundle::check() {
        bundle::fatal(&e);
    }
    let app = tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("oasvc", primitives::ports::tauri_protocol)
        .register_uri_scheme_protocol("oamod", |ctx, request| {
            let r = match ctx.app_handle().try_state::<Arc<Kernel>>() {
                Some(k) => protocol::handle(&k.workspace, &request.uri().to_string()),
                None => protocol::ProtoResponse { status: 503, mime: "text/plain", body: b"kernel not ready".to_vec() },
            };
            tauri::http::Response::builder()
                .status(r.status)
                .header("Content-Type", r.mime)
                .header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*")
                .body(r.body)
                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            send_prompt,
            cancel_turn,
            respond_permission,
            ui_action,
            iframe_call,
            report_ui_error,
            select_harness,
            harness_login,
            harness_login_submit,
            harness_logout,
            web_bounds,
            answer_question,
            save_audio,
            submit_secret,
            channel_subscribe,
            channel_unsubscribe,
            channel_publish,
            primitive_command
        ])
        // Microphone (audio.record, html views) and camera in the shell webviews only (main + module windows); web
        // surfaces get WebKit's prompt. In a shell webview `primitives::media`'s capture gate then checks the asking
        // frame: the shell page, or an `oamod://<module>` view whose module holds the grant. No gate → denied.
        .on_permission_request(|webview, kind| {
            use tauri::webview::{PermissionKind, PermissionResponse};
            let shell = webview.label() == "main" || webview.label().starts_with("win-");
            match (shell, kind) {
                (true, PermissionKind::PointerLock) => PermissionResponse::Allow,
                (true, PermissionKind::Microphone | PermissionKind::Camera) => {
                    if primitives::media::gate_installed() {
                        return PermissionResponse::Allow;
                    }
                    // not wrapped yet (this call came straight from wry): deny it, install the gate for the retry
                    if let Some(k) = webview.try_state::<Arc<Kernel>>() {
                        primitives::media::install_gate(&k);
                    }
                    PermissionResponse::Deny
                }
                (true, _) => PermissionResponse::Deny,
                _ => PermissionResponse::Default,
            }
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(tauri_plugin_log::Builder::default().level(log::LevelFilter::Info).build())?;
            }
            let ws = workspace::resolve_workspace();
            let sink = Arc::new(TauriSink(app.handle().clone()));
            let k = Kernel::new(ws, sink).map_err(|e| format!("workspace init failed: {e}"))?;
            let _ = k.web_host.set(web::TauriWeb::new(app.handle().clone()));
            app.manage(k.clone());
            watch_vm_image(&k);
            // §21 G4/G5: menus, tray, shortcuts, dialogs, clipboard (native shell primitives)
            primitives::shell_misc::attach(&k, app.handle());
            // §21 G4: capture gate for module html views (the main webview exists: wry's delegate class is registered)
            if !primitives::media::install_gate(&k) {
                log::error!("media gate not installed: microphone and camera stay off in OpenApp's webviews");
            }
            tauri::async_runtime::spawn(async move {
                if let Err(e) = k.start(true).await {
                    log::error!("kernel start failed: {e}");
                    k.set_agent("error", Some(format!("kernel start failed: {e}")));
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|handle, event| {
        // §21 G4: openapp:// deep links, "Open with OpenApp" files, dock click
        primitives::shell_misc::on_run_event(handle, &event);
        if let tauri::RunEvent::Exit = event
            && let Some(k) = handle.try_state::<Arc<Kernel>>() {
                k.shutdown();
            }
    });
}
