//! Native web surfaces (§13): one Tauri child webview per mounted web placement, drawn above
//! the shell at the rect the shell reports. Persistent data store per profile.

use crate::util::MutexExt;
use crate::kernel::{Kernel, WebHost, WebRect};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::webview::{DownloadEvent, NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewBuilder, WebviewUrl};

/// Desktop Safari: Google sign-in rejects some embedded-browser user agents.
pub const SAFARI_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15";

pub struct TauriWeb {
    app: AppHandle,
    views: Mutex<HashMap<String, Webview>>,
}

impl TauriWeb {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self { app, views: Mutex::new(HashMap::new()) })
    }

    fn get(&self, placement: &str) -> Option<Webview> {
        self.views.locked().get(placement).cloned()
    }

    fn kernel(app: &AppHandle) -> Option<Arc<Kernel>> {
        app.try_state::<Arc<Kernel>>().map(|s| s.inner().clone())
    }
}

/// Stable 16 bytes from a profile name (FNV-1a, two seeds) for `data_store_identifier`.
pub fn profile_id(profile: &str) -> [u8; 16] {
    let fnv = |seed: u64| {
        let mut h = seed;
        for b in format!("openapp-web-profile:{profile}").bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    };
    let (a, b) = (fnv(0xcbf29ce484222325), fnv(0x84222325cbf29ce4));
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_be_bytes());
    out[8..].copy_from_slice(&b.to_be_bytes());
    out
}

fn label_for(placement: &str) -> String {
    format!("web-{placement}")
}

impl WebHost for TauriWeb {
    fn set_bounds(&self, placement: &str, url: &str, profile: &str, rect: Option<WebRect>) -> Result<(), String> {
        let existing = self.get(placement);
        let Some(r) = rect else {
            if let Some(w) = existing {
                log::info!("web: hide {placement}");
                w.hide().map_err(|e| e.to_string())?;
            }
            return Ok(());
        };
        let pos = LogicalPosition::new(r.x, r.y);
        let size = LogicalSize::new(r.width.max(1.0), r.height.max(1.0));
        if let Some(w) = existing {
            log::debug!("web: move {placement} to {r:?}");
            w.set_position(pos).map_err(|e| e.to_string())?;
            w.set_size(size).map_err(|e| e.to_string())?;
            w.show().map_err(|e| e.to_string())?;
            return Ok(());
        }
        // §21 G4: web views in module windows (`window:<id>`) live in that window
        let label = crate::primitives::shell_windows::window_label_for_placement(placement);
        let window = self.app.get_window(&label).ok_or_else(|| format!("window {label} not found"))?;
        let parsed: tauri::Url = url.parse().map_err(|e| format!("bad url {url}: {e}"))?;
        let p_load = placement.to_string();
        let p_title = placement.to_string();
        let app_load = self.app.clone();
        let app_title = self.app.clone();
        let app_dl = self.app.clone();
        let p_dl = placement.to_string();
        let mut builder = WebviewBuilder::new(label_for(placement), WebviewUrl::External(parsed))
            .user_agent(SAFARI_UA)
            .on_page_load(move |_w, payload| {
                if let Some(k) = TauriWeb::kernel(&app_load) {
                    let loading = matches!(payload.event(), PageLoadEvent::Started);
                    k.web_update(&p_load, Some(payload.url().as_str()), None, Some(loading));
                }
            })
            .on_document_title_changed(move |_w, title| {
                if let Some(k) = TauriWeb::kernel(&app_title) {
                    k.web_update(&p_title, None, Some(&title), None);
                }
            })
            // sign-in popups (OAuth) open with the default implementation
            .on_new_window(|_url, _features| NewWindowResponse::Allow)
            // §20.3: downloads go to a kernel temp dir first (OAuth client JSON → Keychain card; else the module's data/downloads)
            .on_download(move |_w, event| {
                let Some(k) = TauriWeb::kernel(&app_dl) else { return false };
                match event {
                    DownloadEvent::Requested { url, destination } => {
                        let suggested = destination.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                        *destination = k.web_download_dest(&p_dl, url.as_str(), &suggested);
                        log::info!("web: download in {p_dl} from {}", url.as_str().chars().take(80).collect::<String>());
                        true
                    }
                    DownloadEvent::Finished { url, success, .. } => {
                        k.web_download_done(url.as_str(), success);
                        true
                    }
                    _ => true,
                }
            });
        #[cfg(target_os = "macos")]
        {
            builder = builder.data_store_identifier(profile_id(profile));
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Ok(dir) = self.app.path().app_data_dir() {
                builder = builder.data_directory(dir.join("web-profiles").join(profile));
            }
        }
        log::info!("web: create {placement} profile={profile} url={url} at {r:?}");
        let w = window.add_child(builder, pos, size).map_err(|e| format!("could not create web view: {e}"))?;
        self.views.locked().insert(placement.to_string(), w);
        Ok(())
    }

    fn navigate(&self, placement: &str, url: &str) -> Result<(), String> {
        let w = self.get(placement).ok_or("web view is not mounted")?;
        w.navigate(url.parse().map_err(|e| format!("bad url {url}: {e}"))?).map_err(|e| e.to_string())
    }

    fn history(&self, placement: &str, step: i32) -> Result<(), String> {
        let w = self.get(placement).ok_or("web view is not mounted")?;
        match step {
            0 => w.reload().map_err(|e| e.to_string()),
            -1 => w.eval("history.back()").map_err(|e| e.to_string()),
            _ => w.eval("history.forward()").map_err(|e| e.to_string()),
        }
    }

    fn eval(&self, placement: &str, js: &str, done: Box<dyn FnOnce(String) + Send>) -> Result<(), String> {
        let w = self.get(placement).ok_or("web view is not mounted")?;
        let once = Mutex::new(Some(done));
        w.eval_with_callback(js, move |s| {
            if let Some(f) = once.locked().take() {
                f(s);
            }
        })
        .map_err(|e| e.to_string())
    }

    fn destroy(&self, placement: &str) {
        if let Some(w) = self.views.locked().remove(placement) {
            log::info!("web: destroy {placement}");
            let _ = w.close();
        }
    }
}
