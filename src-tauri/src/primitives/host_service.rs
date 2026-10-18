//! G8 long-running host modules (contract §21, §18.1).
//! - `"host": { "swift": "host/main.swift", "summary": "…", "service": true, "windows": true }`: the host part
//!   starts with the module (G9: such modules load eagerly and never idle-stop) and is supervised: when the
//!   process exits it is started again (backoff 1 s … 30 s; the sidecar's 5-starts-per-minute cap applies),
//!   research `host_service_exit`. Channels (G1) work as for every host module.
//! - Native windows: a host module is an NSApplication accessory process. Sources that use `OpenAppHost` get the
//!   kernel runtime (`OpenAppHost.swift`, compiled alongside; JSON-RPC on a background thread, handlers on the
//!   main thread, `OpenAppHost.run()` starts the AppKit run loop). The consent card says "opens its own windows"
//!   when the code uses NSWindow/NSPanel/openWindow or the manifest sets `"windows": true`.
//! - Host data / describe `host_services` list each service module (running, pid, restarts).

use crate::research::Kind;
use crate::util::MutexExt;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The kernel's AppKit + JSON-RPC runtime for host modules (compiled next to sources that use `OpenAppHost`).
pub const RUNTIME_SWIFT: &str = include_str!("OpenAppHost.swift");

pub fn is_service(mf: &crate::modules::Manifest) -> bool {
    mf.raw.get("host").and_then(|h| h.get("service")).and_then(Value::as_bool) == Some(true) && mf.host.is_some()
}

pub fn declares_windows(mf_raw: &Value) -> bool {
    mf_raw.get("host").and_then(|h| h.get("windows")).and_then(Value::as_bool) == Some(true)
}

#[derive(Default)]
struct HostSvc {
    /// module → (restarts, last exit time)
    restarts: Mutex<HashMap<String, u32>>,
    supervising: Mutex<std::collections::HashSet<String>>,
    started: AtomicBool,
}

fn st(k: &Kernel) -> Arc<HostSvc> {
    k.prims.get::<HostSvc>()
}

fn current(k: &Kernel, id: &str) -> Option<Arc<crate::host_services::Sidecar>> {
    k.lock().modules.get(id).filter(|m| m.status == "running").and_then(|m| m.host.clone())
}

async fn supervise(w: std::sync::Weak<Kernel>, id: String, sc: Arc<crate::host_services::Sidecar>) {
    let mut backoff = Duration::from_secs(1);
    let mut was_running = sc.is_running();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(k) = w.upgrade() else { break };
        if k.is_shutdown() {
            break;
        }
        // reload / unload replaced or removed the sidecar: this supervisor is done
        match current(&k, &id) {
            Some(c) if Arc::ptr_eq(&c, &sc) => {}
            _ => break,
        }
        if sc.is_running() {
            was_running = true;
            backoff = Duration::from_secs(1);
            continue;
        }
        if was_running {
            let n = {
                let s = st(&k);
                let mut r = s.restarts.locked();
                let n = r.entry(id.clone()).or_default();
                *n += 1;
                *n
            };
            k.research.log(&format!("host:{id}"), Kind::HostServiceExit, json!({ "restarts": n, "tail": crate::modules::tail_lines(&sc.tail, 3).len() }));
            was_running = false;
            k.emit_snapshot();
        }
        drop(k);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
        // `methods` (re)spawns the process; the module's own start code runs again
        match sc.call("methods", json!({}), Duration::from_secs(15)).await {
            Ok(_) => {
                if let Some(k) = w.upgrade() {
                    k.research.log(&format!("host:{id}"), Kind::HostServiceStart, json!({}));
                    k.emit_snapshot();
                }
            }
            Err(e) => log::warn!("host service {id}: {e}"),
        }
    }
    if let Some(k) = w.upgrade() {
        st(&k).supervising.locked().remove(&id);
    }
}

fn on_loaded(k: &Kernel, id: &str) {
    let Some(mf) = k.lock().modules.get(id).and_then(|m| m.manifest.clone()) else { return };
    if !is_service(&mf) {
        return;
    }
    let Some(sc) = current(k, id) else { return };
    let Some(ka) = k.arc() else { return };
    st(k).supervising.locked().insert(id.to_string());
    k.spawn(supervise(Arc::downgrade(&ka), id.to_string(), sc));
}

fn statuses(k: &Kernel) -> Vec<Value> {
    let restarts = st(k).restarts.locked().clone();
    k.lock()
        .modules
        .values()
        .filter(|m| m.manifest.as_ref().is_some_and(is_service))
        .map(|m| {
            let running = m.host.as_ref().is_some_and(|h| h.is_running());
            json!({ "id": m.id, "title": m.name, "subtitle": format!("Mac service · {}{}", if running { "running" } else { m.status.as_str() }, restarts.get(&m.id).map(|n| format!(" · {n} restarts")).unwrap_or_default()),
                "icon": "cpu", "module": m.id, "running": running, "pid": m.host.as_ref().and_then(|h| h.pid()), "restarts": restarts.get(&m.id).copied().unwrap_or(0),
                "windows": m.manifest.as_ref().is_some_and(|mf| declares_windows(&mf.raw)) })
        })
        .collect()
}

pub fn primitive() -> Primitive {
    Primitive {
        name: "host_service",
        purpose: "Long-running host modules on the Mac (\"service\": true, supervised and restarted) with live channels and their own AppKit windows (OpenAppHost runtime), e.g. an AVAudioEngine engine with meters at 60 Hz and Audio Unit plugin windows.",
        example: r#"openapp.json: "host": { "swift": "host/main.swift", "summary": "Plays and records audio on your Mac.", "service": true, "windows": true } · Swift: OpenAppHost.method("open_plugin") { _ in OpenAppHost.openWindow(title: "Synth", content: view) }; OpenAppHost.publish("host:daw/meters", ["l": 0.4]); OpenAppHost.run()"#,
        class: Class::Granted,
        data: vec![DataSource { name: "host_module_services", doc: "list items [{ id: module, title, subtitle, icon, running, pid, restarts, windows }] — long-running host modules", get: data_fn(|k| Value::Array(statuses(k))) }],
        on_event: Some(Arc::new(|k, ev| {
            if let Event::ModuleLoaded(id) = ev {
                on_loaded(k, id);
            }
            if let Event::Started = ev {
                st(k).started.store(true, Ordering::Relaxed);
            }
        })),
        describe_keys: vec![("host_module_services", data_fn(|k| json!({
            "services": statuses(k),
            "runtime": "sources that use OpenAppHost get the kernel runtime: OpenAppHost.method(name, description) { params in … } (main thread, async ok), .publish(topic, data), .subscribe(topic) { data in … }, .openWindow(title:width:height:content:) → id, .closeWindow(id), .run() (AppKit run loop; never returns). Built-in method openapp.windows → { count, titles }.",
        })))],
        ..Default::default()
    }
}
