//! §15.1 audio: microphone consent per module and recordings handed to a module tool (or transcribed on the Mac).
//! The shell records (`audio.record` in a view), asks [`consent`] first, then sends the bytes to [`save`].

use crate::research::Kind;
use super::Builtin;
use crate::consent::{ConsentReq, Granted};
use crate::kernel::{Kernel, base64_decode};
use crate::modules;
use serde_json::{Value, json};
use std::sync::Arc;

/// `audio.record` runs from a view control: the shell records, then calls `save_audio`.
pub const ACTIONS: &[Builtin] = &[Builtin {
    name: "audio.record",
    args: "",
    keys: &["tool"],
    run: |_, _| Err("audio.record runs from a view control (the app records, then hands the file to the module tool)".into()),
}];

/// First use per module asks with a kernel consent card; the decision is stored (grant `microphone:<module>`).
pub async fn consent(k: &Kernel, module: &str) -> Result<(), String> {
    let name = k.lock().modules.get(module).map(|m| m.name.clone()).unwrap_or_else(|| module.to_string());
    let req = ConsentReq::new("microphone", format!("{name} wants to use the microphone"), "OpenApp records only while the control is active and hands the recording to this module.")
        .info(json!({ "module": module }));
    match crate::consent::granted(k, module, req, true, Value::Null).await {
        Granted { allowed: true, .. } => Ok(()),
        Granted { asked: false, .. } => Err(format!("microphone access for \"{module}\" was denied")),
        Granted { asked: true, .. } => Err(format!("microphone access for \"{module}\" was not allowed")),
    }
}

/// Shell command `save_audio`: write the recording into the module's data folder and call
/// the module tool from the placement's `audio.record` action with `{ [arg]: path, mime }`.
pub async fn save(
    k: &Arc<Kernel>,
    placement_id: &str,
    data: Value,
    mime: &str,
    action: Option<Value>,
    duration_ms: Option<u64>,
) -> Result<Value, String> {
    let (module, view) = k.placement_view(placement_id).ok_or_else(|| format!("unknown placement \"{placement_id}\""))?;
    if k.grants.allowed("microphone", &module) != Some(true) {
        return Err(format!("microphone access for \"{module}\" is not allowed"));
    }
    let action = match action.filter(|a| a.get("tool").is_some_and(|t| t.is_string()) || a.get("transcribe").is_some_and(|t| t.is_string())) {
        Some(a) => a,
        None => {
            let g = k.lock();
            let tree = g.modules.get(&module).and_then(|m| m.views.get(&view)).and_then(|v| v.tree()).cloned();
            tree.as_ref().and_then(find_audio_action).ok_or("no audio.record action with a \"tool\" in this view")?
        }
    };
    let transcribe = action["transcribe"].as_str().map(String::from);
    if transcribe.as_deref().is_some_and(|t| t != "apple") {
        return Err(format!("audio.record transcribe must be \"apple\", got {:?}", transcribe));
    }
    let tool = action["tool"].as_str().map(String::from).unwrap_or_default();
    let insert = action["insert"].as_bool().unwrap_or(false);
    if tool.is_empty() && !(transcribe.is_some() && insert) {
        return Err("audio.record needs \"tool\", or transcribe: \"apple\" with insert: true".into());
    }
    let arg = action["arg"].as_str().unwrap_or("path").to_string();
    let bytes = match &data {
        Value::String(s) => base64_decode(s.split(',').next_back().unwrap_or(s)).ok_or("data is not valid base64")?,
        Value::Array(a) => a.iter().map(|v| v.as_u64().filter(|n| *n < 256).map(|n| n as u8)).collect::<Option<Vec<u8>>>().ok_or("data must be bytes")?,
        _ => return Err("data must be a base64 string or a byte array".into()),
    };
    if bytes.is_empty() {
        return Err("the recording is empty".into());
    }
    let ext = match mime.split(';').next().unwrap_or("") {
        "audio/webm" | "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" => "mp3",
        _ => "bin",
    };
    let dir = k.module_dir(&module).join("data").join("recordings");
    let path = dir.join(format!("{}.{ext}", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
    // §17: a module server in the VM gets the file through the bridge (the shared mount shows
    // host writes to the guest only after ~5 s); the guest write reaches the host at once.
    let in_vm = k.lock().modules.get(&module).is_some_and(|m| m.in_vm);
    let tool_path = match (in_vm, k.vm()) {
        (true, Some(vm)) => {
            let gp = vm.guest_path(&path).ok_or("recording path is outside the workspace")?;
            vm.write_file(&gp, &bytes).await?;
            gp
        }
        (true, None) => return Err("the app VM is not running".into()),
        (false, _) => {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
            path.to_string_lossy().to_string()
        }
    };
    k.research.log(&format!("module:{module}"), Kind::AudioRecord, json!({ "bytes": bytes.len(), "mime": mime, "duration_ms": duration_ms, "tool": tool, "via": if in_vm { "vm-bridge" } else { "host" } }));
    // §18: on-device transcription on the Mac (SpeechAnalyzer), then insert or hand the text to a tool
    let mut transcript: Option<Value> = None;
    if transcribe.is_some() {
        let mut p = json!({ "path": path.to_string_lossy() });
        if let Some(l) = action["locale"].as_str() {
            p["locale"] = json!(l);
        }
        let r = k.host_call(None, "speech.transcribe", p, &format!("module:{module}")).await.inspect_err(|e| {
            let hint = if ext == "webm" { " (WebKit recorded webm; macOS needs audio/mp4)" } else { "" };
            k.push_ui_error(placement_id, Some(&module), &format!("transcription failed{hint}: {e}"));
        })?;
        if insert {
            let text = r["text"].as_str().unwrap_or("").to_string();
            if !text.is_empty() {
                k.sink.emit("oa://composer", json!({ "insert": text }));
            }
            if tool.is_empty() {
                return Ok(json!({ "text": text, "locale": r["locale"], "path": tool_path }));
            }
        }
        transcript = Some(r);
    }
    let mut args = action.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(t) = &transcript {
        args.insert("text".into(), t["text"].clone());
        args.insert("locale".into(), t["locale"].clone());
        args.insert("path".into(), json!(tool_path));
    } else {
        args.insert(arg, json!(tool_path));
    }
    args.insert("mime".into(), json!(mime));
    let r = k.call_module_tool(&module, &tool, args).await.inspect_err(|e| {
        k.push_ui_error(placement_id, Some(&module), &format!("audio tool {tool} failed: {e}"));
    })?;
    k.after_module_call(&module, &r, true);
    if r.is_error == Some(true) {
        let msg = modules::result_text(&r);
        k.push_ui_error(placement_id, Some(&module), &format!("audio tool {tool} returned an error: {msg}"));
        return Err(msg);
    }
    serde_json::to_value(&r).map_err(|e| e.to_string())
}

fn find_audio_action(n: &Value) -> Option<Value> {
    for key in ["action", "submit"] {
        if let Some(a) = n.get(key)
            && a["host"] == "audio.record" && (a.get("tool").is_some() || a.get("transcribe").is_some()) {
                return Some(a.clone());
            }
    }
    n.get("children").and_then(Value::as_array)?.iter().find_map(find_audio_action)
}
