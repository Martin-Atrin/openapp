//! §21 G4 microphone + camera in module html views (`oamod://<module>/…` iframes).
//!
//! - Consent: `media.request { audio?, video? }` (and the older `media.camera`) asks once per module and kind
//!   (grants `microphone:<module>`, `camera:<module>`; Allow / Deny, remembered). The bridge runs it before every
//!   `navigator.mediaDevices.getUserMedia` of the page and exposes `openapp.requestMedia(constraints)` →
//!   `MediaStream`, so a page never sees a capture prompt before the card was answered.
//! - Enforcement: the WKWebView capture callback (`webView:requestMediaCapturePermissionForOrigin:…`, the method
//!   wry's UI delegate implements) is wrapped once at startup ([`install_gate`]). wry / Tauri decide first (shell
//!   webview → allow, web surface → WebKit's own prompt); for a shell webview the gate then looks at the frame
//!   that asks ([`decide`]): the shell page itself → allow (unchanged), `oamod://<module>` → allow only with that
//!   module's grant (no grant yet → deny now and show the card; the page retries after Allow), any other frame
//!   (an external site embedded in a view) → deny. Without the gate (it failed to install) shell capture is denied.

use super::shell;
use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn primitive() -> Primitive {
    Primitive {
        name: "media",
        purpose: "Microphone and camera in a module's html views: one consent card per module and kind, then getUserMedia works in the view",
        example: r#"{ "host": "media.request", "audio": true } · in an html view: const stream = await openapp.requestMedia({ audio: true })"#,
        class: Class::Granted,
        actions: vec![
            Action {
                name: "media.request",
                args: "{ audio?: bool, video?: bool } → { audio, video } granted (one card per module and kind); then the module's html views may call getUserMedia",
                handler: handler(|k, p, c| async move { request(&k, &p, &c).await }),
            },
            Action {
                name: "media.camera",
                args: "{} → asks once per module; then the module's html views may call getUserMedia({ video })",
                handler: handler(|k, p, c| async move { request(&k, &json!({ "module": p.get("module"), "video": true }), &c).await }),
            },
        ],
        ..Default::default()
    }
}

/// Grant kind of a capture kind (`audio` → `microphone`, `video` → `camera`).
fn grant_kind(audio: bool) -> &'static str {
    if audio { "microphone" } else { "camera" }
}

async fn request(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, HostError> {
    let module = shell::owner(k, caller, p)?;
    let audio = p.get("audio").and_then(Value::as_bool).unwrap_or(false);
    let video = p.get("video").and_then(Value::as_bool).unwrap_or(false);
    if !audio && !video {
        return Err(HostError::bad_request("media.request needs \"audio\": true and / or \"video\": true"));
    }
    for (want, kind) in [(audio, "microphone"), (video, "camera")] {
        if want {
            ask(k, kind, &module).await?;
        }
    }
    Ok(json!({ "ok": true, "audio": audio, "video": video }))
}

/// One card per (kind, module) at a time; a second caller waits and then reads the stored answer.
async fn ask(k: &Arc<Kernel>, kind: &str, module: &str) -> Result<(), HostError> {
    static ASKING: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);
    let lock = ASKING.locked().get_or_insert_with(HashMap::new).entry(format!("{kind}:{module}")).or_default().clone();
    let _g = lock.lock().await;
    let name = k.lock().modules.get(module).map(|m| m.name.clone()).unwrap_or_else(|| module.to_string());
    let (title, text) = if kind == "camera" {
        (format!("{name} wants to use the camera"), "Its views can then turn on the camera while they are open (the camera light shows when it is on). macOS asks once more for OpenApp.")
    } else {
        (format!("{name} wants to use the microphone"), "Its views can then record audio while they are open (macOS shows the microphone indicator). macOS asks once more for OpenApp.")
    };
    crate::consent::module_grant(k, kind, module, &title, text).await
}

/// What the gate does with a capture request from a shell webview.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// deny this request and show the consent card for these grant kinds of the module
    Ask(String, Vec<&'static str>),
}

/// Pure decision for a capture request in a shell webview. `main_frame`: the shell page itself; `scheme` / `host`:
/// the asking frame's security origin; `audio` / `video`: what it wants; `allowed(kind, module)`: the grant store.
pub fn decide(main_frame: bool, scheme: &str, host: &str, audio: bool, video: bool, allowed: impl Fn(&str, &str) -> Option<bool>) -> Decision {
    if main_frame {
        return Decision::Allow;
    }
    if !scheme.eq_ignore_ascii_case("oamod") || host.is_empty() {
        return Decision::Deny;
    }
    let module = host.to_ascii_lowercase();
    let kinds: Vec<&'static str> = [(audio, true), (video, false)].iter().filter(|(w, _)| *w).map(|(_, a)| grant_kind(*a)).collect();
    if kinds.is_empty() {
        return Decision::Deny;
    }
    let states: Vec<Option<bool>> = kinds.iter().map(|kd| allowed(kd, &module)).collect();
    if states.iter().all(|s| *s == Some(true)) {
        return Decision::Allow;
    }
    if states.contains(&Some(false)) {
        return Decision::Deny;
    }
    let missing = kinds.iter().zip(&states).filter(|(_, s)| s.is_none()).map(|(kd, _)| *kd).collect();
    Decision::Ask(module, missing)
}

static GATE: AtomicBool = AtomicBool::new(false);

/// The capture gate is installed (else shell webviews deny capture: fail closed).
pub fn gate_installed() -> bool {
    GATE.load(Ordering::Acquire)
}

/// Install the capture gate (main thread, after the first webview exists; idempotent). Returns whether it is on.
pub fn install_gate(k: &Arc<Kernel>) -> bool {
    if gate_installed() {
        return true;
    }
    let weak = Arc::downgrade(k);
    let decide_fn = move |main_frame: bool, scheme: &str, host: &str, audio: bool, video: bool| -> bool {
        let Some(k) = weak.upgrade() else { return false };
        let d = decide(main_frame, scheme, host, audio, video, |kind, m| k.grants.allowed(kind, m));
        if !main_frame {
            k.research.log("kernel", crate::research::Kind::Permission, json!({ "phase": "media", "origin": format!("{scheme}://{host}"), "audio": audio, "video": video, "decision": format!("{d:?}") }));
        }
        match d {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask(module, kinds) => {
                tauri::async_runtime::spawn(async move {
                    for kind in kinds {
                        if ask(&k, kind, &module).await.is_err() {
                            break;
                        }
                    }
                });
                false
            }
        }
    };
    let ok = gate::install(Box::new(decide_fn));
    GATE.store(ok, Ordering::Release);
    ok
}

#[cfg(not(target_os = "macos"))]
mod gate {
    pub fn install(_f: Box<dyn Fn(bool, &str, &str, bool, bool) -> bool + Send + Sync>) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod gate {
    use block2::{Block, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::NSString;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::OnceLock;

    type DecideFn = Box<dyn Fn(bool, &str, &str, bool, bool) -> bool + Send + Sync>;
    /// `webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:`; `type` is
    /// WKMediaCaptureType (0 camera, 1 microphone, 2 both), the handler takes WKPermissionDecision (0 prompt, 1 grant, 2 deny).
    type CaptureFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject, *mut AnyObject, isize, *mut Block<dyn Fn(isize)>);
    const GRANT: isize = 1;
    const DENY: isize = 2;

    static ORIG: OnceLock<Imp> = OnceLock::new();
    static DECIDE: OnceLock<DecideFn> = OnceLock::new();

    fn ns(o: Option<Retained<NSString>>) -> String {
        o.map(|s| s.to_string()).unwrap_or_default()
    }

    unsafe extern "C-unwind" fn gated(
        this: *mut AnyObject,
        cmd: Sel,
        webview: *mut AnyObject,
        origin: *mut AnyObject,
        frame: *mut AnyObject,
        ty: isize,
        handler: *mut Block<dyn Fn(isize)>,
    ) {
        let (Some(orig), Some(decide)) = (ORIG.get(), DECIDE.get()) else {
            // SAFETY: WebKit passes a valid decision block.
            unsafe { (*handler).call((DENY,)) };
            return;
        };
        // wry + Tauri first (shell webview → grant, web surface → prompt / deny); wry answers synchronously
        let got = Rc::new(Cell::new(None::<isize>));
        let g2 = got.clone();
        let inner = RcBlock::new(move |d: isize| g2.set(Some(d)));
        // SAFETY: ORIG is the original implementation of this very selector (same signature).
        let orig: CaptureFn = unsafe { std::mem::transmute::<Imp, CaptureFn>(*orig) };
        unsafe { orig(this, cmd, webview, origin, frame, ty, &*inner as *const Block<dyn Fn(isize)> as *mut _) };
        let base = got.get().unwrap_or(DENY);
        let decision = if base != GRANT || frame.is_null() {
            if frame.is_null() { DENY } else { base }
        } else {
            // SAFETY: frame is a WKFrameInfo; securityOrigin is a WKSecurityOrigin (protocol, host: NSString).
            let (main, scheme, host) = unsafe {
                let main: bool = msg_send![frame, isMainFrame];
                let so: *mut AnyObject = msg_send![frame, securityOrigin];
                let src = if so.is_null() { origin } else { so };
                if src.is_null() {
                    (main, String::new(), String::new())
                } else {
                    let p: Option<Retained<NSString>> = msg_send![src, protocol];
                    let h: Option<Retained<NSString>> = msg_send![src, host];
                    (main, ns(p), ns(h))
                }
            };
            let (audio, video) = match ty {
                0 => (false, true),
                1 => (true, false),
                2 => (true, true),
                _ => (false, false),
            };
            if (audio || video || main) && decide(main, &scheme, &host, audio, video) { GRANT } else { DENY }
        };
        // SAFETY: WebKit passes a valid decision block; it is called exactly once.
        unsafe { (*handler).call((decision,)) };
    }

    pub fn install(f: DecideFn) -> bool {
        if ORIG.get().is_some() {
            return true;
        }
        let Some(cls) = AnyClass::get(c"WryWebViewUIDelegate") else {
            log::warn!("media gate: wry's WKUIDelegate class is not registered yet");
            return false;
        };
        let s = sel!(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:);
        let Some(m) = cls.instance_method(s) else {
            log::error!("media gate: WryWebViewUIDelegate has no media capture callback; capture in shell webviews stays denied");
            return false;
        };
        let _ = DECIDE.set(f);
        // SAFETY: `gated` has the exact signature of the replaced method (checked against wry 0.57's delegate).
        let new: Imp = unsafe { std::mem::transmute::<CaptureFn, Imp>(gated) };
        let old = unsafe { m.set_implementation(new) };
        let _ = ORIG.set(old);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_module_grant_decision() {
        let grants = |kind: &str, m: &str| match (kind, m) {
            ("microphone", "music") => Some(true),
            ("camera", "music") => None,
            ("microphone", "blocked") => Some(false),
            ("camera", "video") => Some(true),
            _ => None,
        };
        // the shell page itself: unchanged
        assert_eq!(decide(true, "tauri", "localhost", true, false, grants), Decision::Allow);
        assert_eq!(decide(true, "http", "localhost", true, true, grants), Decision::Allow);
        // module iframes: only with the module's grant
        assert_eq!(decide(false, "oamod", "music", true, false, grants), Decision::Allow);
        assert_eq!(decide(false, "OAMOD", "Music", true, false, grants), Decision::Allow);
        assert_eq!(decide(false, "oamod", "video", false, true, grants), Decision::Allow);
        assert_eq!(decide(false, "oamod", "music", true, true, grants), Decision::Ask("music".into(), vec!["camera"]));
        assert_eq!(decide(false, "oamod", "fresh", true, true, grants), Decision::Ask("fresh".into(), vec!["microphone", "camera"]));
        assert_eq!(decide(false, "oamod", "blocked", true, false, grants), Decision::Deny);
        // nothing asked, opaque origin, external sites embedded in a view: never
        assert_eq!(decide(false, "oamod", "music", false, false, grants), Decision::Deny);
        assert_eq!(decide(false, "oamod", "", true, false, grants), Decision::Deny);
        assert_eq!(decide(false, "https", "music", true, false, grants), Decision::Deny);
        assert_eq!(decide(false, "https", "evil.example", true, false, |_, _| Some(true)), Decision::Deny);
    }
}
