//! `openapp_describe` and `openapp_snapshot` (the text tree of what the user sees).

use super::*;
use crate::modules;

pub(crate) fn region_title(r: &layout::Region) -> String {
    format!(
        "region:{} — {} edge, {}px, {}{}",
        r.id,
        r.edge,
        r.size,
        r.mode,
        r.title.as_ref().map(|t| format!(", title \"{t}\"")).unwrap_or_default()
    )
}

pub(crate) fn render_anchor(g: &Inner, anchor: &str, indent: usize, out: &mut String, rendered: &mut BTreeSet<String>, depth: usize) {
    let pad = "  ".repeat(indent);
    let mut ps: Vec<_> = g.layout.placements.iter().filter(|p| p.anchor == anchor).collect();
    ps.sort_by(|a, b| a.order.partial_cmp(&b.order).unwrap_or(std::cmp::Ordering::Equal));
    if ps.is_empty() {
        out.push_str(&format!("{pad}(empty)\n"));
        return;
    }
    for p in ps {
        if !rendered.insert(p.id.clone()) || depth > 8 {
            out.push_str(&format!("{pad}- {} (already shown / nested too deep)\n", p.id));
            continue;
        }
        let label = p.label.as_ref().map(|l| format!(" label \"{l}\"")).unwrap_or_default();
        let view = g.modules.get(&p.module).and_then(|m| m.views.get(&p.view).map(|v| (m, v)));
        match view {
            None => {
                let why = match g.modules.get(&p.module) {
                    None => "module not found".to_string(),
                    Some(m) if m.manifest.is_none() => format!("module {} (not loaded)", m.status),
                    Some(_) => "view not in manifest".to_string(),
                };
                out.push_str(&format!("{pad}- {} {}/{}{label}: !! {why}\n", p.id, p.module, p.view));
            }
            Some((m, v)) => {
                let status = if m.status != "running" { format!(" [module {}]", m.status) } else { String::new() };
                match v.value.get("kind").and_then(Value::as_str) {
                    Some("spec") => {
                        out.push_str(&format!("{pad}- {} {}/{} (spec){label}{status}\n", p.id, p.module, p.view));
                        let tree = crate::primitives::channels::resolve_tree_tls(&v.value.get("tree").cloned().unwrap_or(Value::Null));
                        let mut slot_fn = |id: &str, ind: usize, o: &mut String| {
                            render_anchor(g, &format!("slot:{id}"), ind, o, rendered, depth + 1);
                        };
                        views::render(&tree, indent + 1, out, &mut slot_fn);
                    }
                    Some("web") => out.push_str(&format!(
                        "{pad}- {} {}/{} (web {} profile {}{}){label}{status}\n",
                        p.id,
                        p.module,
                        p.view,
                        g.web.get(&p.id).and_then(|w| w["url"].as_str()).unwrap_or(v.value["url"].as_str().unwrap_or("")),
                        v.value["profile"].as_str().unwrap_or(""),
                        g.web.get(&p.id).and_then(|w| w["title"].as_str()).filter(|t| !t.is_empty()).map(|t| format!(", title \"{t}\"")).unwrap_or_default()
                    )),
                    Some("html") => out.push_str(&format!(
                        "{pad}- {} {}/{} (html iframe {} rev {}){label}{status}\n",
                        p.id,
                        p.module,
                        p.view,
                        v.value["url"].as_str().unwrap_or(""),
                        v.rev
                    )),
                    _ => out.push_str(&format!(
                        "{pad}- {} {}/{}{label}: !! view error: {}\n",
                        p.id,
                        p.module,
                        p.view,
                        v.value["error"].as_str().unwrap_or("")
                    )),
                }
            }
        }
    }
}

/// True kernel gaps: things the agent cannot build from inside its world (everything else it builds),
/// each with the closest alternative.
pub const KERNEL_GAPS: &[&str] = &[
    "reading other apps' stored secrets (browser cookies, other apps' Keychain items, mail passwords): use secret.request or oauth.authorize instead",
    "HomeKit (needs an Apple entitlement this unsigned build lacks): run the user's Shortcuts from a host module (`shortcuts run`), or the device vendor's / Home Assistant's HTTP API",
    "VPN and network extensions (NetworkExtension entitlement): run a userspace client (e.g. WireGuard, Tailscale) as a service in the VM, or point the user to the vendor's app",
    "Share and Finder extensions (appex targets): use \"opens\" file types, drag and drop, deep links and the share host action",
    "passkeys in web views (WebAuthn entitlement): sign in with password + code in the web view, or OAuth in the system browser (oauth.authorize)",
    "an iframe of an outside web page (use a web view)",
];

pub(crate) fn capabilities(web: bool, vm: bool) -> Value {
    json!({
        "vm": vm,
        "egress": if vm { "proxy+consent" } else { "host network (the provider runs outside the VM)" },
        "views": {
            "spec": "native themed UI from the catalog (see spec_catalog)",
            "html": "any UI in a sandboxed iframe with window.openapp (call, host, onTheme)",
            "web": if web { "real web pages in a native browser surface with persistent login profiles; host actions web.navigate/back/forward/reload; host data web:<placement>" } else { "not available in this build (headless)" }
        },
        "module_servers": if vm {
            "MCP servers in any language, started by the kernel inside the app VM (Linux, cwd /workspace/modules/<id>; node, python3, ffmpeg available; network through OpenApp's proxy, which may ask the user)"
        } else {
            "MCP servers in any language, started by the kernel on this Mac (they may use the host's tools, the network, files in their module folder)"
        },
        "host_actions": crate::primitives::host_actions(),
        "audio": "audio.record: the app records from the microphone (user consent per module, AAC .m4a) and calls your module tool with { path, mime }; with transcribe: \"apple\" the Mac transcribes on device and either inserts the text (insert: true) or calls your tool with { text, path, locale }",
        "host_services": "macOS on-device standard library (speech, Apple Intelligence model, OCR, TTS, translation, embeddings) — see host_services",
        "host_modules": "Swift on the Mac for anything else in macOS (\"host\" in openapp.json; user consent per source)",
        "presentations": "view.open as sheet | modal | inline | tab; view.close",
        "turn_undo": "each build turn can be undone by the user (turn.undo)",
        "sessions": "kernel-owned conversations; host data `sessions`; session.new/open/rename",
        "providers": "agent providers; host data `harnesses`; harness.select/refresh",
        "theme": "tokens with contrast checks (openapp_theme)",
        "layout": "regions on any edge (stack or tabs), anchors composer.actions/composer.above/main, slots inside spec views",
        "directives": "tool results may return structuredContent.openapp { composer_insert, prompt, notify }",
        "open_url": "opens http(s) links in the system browser",
        "connect": "secrets in the macOS Keychain (secret.request, never ask for tokens in chat), net.fetch / net.imap on the Mac with credential injection, OAuth (oauth.authorize PKCE loopback, oauth.device) with presets google, github, microsoft, slack, notion, dropbox, spotify, guided consoles (web.read/click/fill/highlight + guide.set), web-view downloads captured (OAuth client JSON → Keychain)"
    })
}

impl Kernel {
    /// describe → `host_services` (live availability) and MCP `host_status`.
    pub fn host_services_info(&self) -> Value {
        let status = self.host_status.locked().clone();
        let modules: Vec<Value> = self
            .lock()
            .modules
            .values()
            .filter(|m| m.host.is_some())
            .map(|m| json!({ "module": m.id, "running": m.host.as_ref().is_some_and(|h| h.is_running()), "methods": m.host_methods }))
            .collect();
        json!({
            "class": "free: on-device (Neural Engine), no consent needed, research-logged without content",
            "status": status,
            "methods": {
                "speech.transcribe": "{ path, locale? } → { text, segments[{text,start,end}], locale } — m4a/aac/wav/caf (not webm)",
                "ai.generate": "{ prompt, instructions?, schema? (JSON Schema → guided JSON), temperature? } → { text } | { json }",
                "ai.status": "{} → { available, reason? }",
                "vision.ocr": "{ path, languages? } → { text, lines[{text, box, confidence}] }",
                "tts.speak": "{ text, voice?, rate? } → plays on the Mac",
                "tts.render": "{ text, path (.m4a/.caf/.wav/.aiff), voice? } → { path, seconds }",
                "translate": "{ text, from?, to } → { text } — only pairs installed on the Mac",
                "nl.embed": "{ texts[], language? } → { vectors[][], dimension }",
                "nl.language": "{ text } → { language, confidence }"
            },
            "paths": "workspace paths (/workspace/... in the VM, or workspace-relative); anything outside the workspace is refused",
            "from_agent": "MCP tools host_* (host_ai_generate, host_speech_transcribe, …) and host_call { module?, method, params }",
            "from_module_servers": "POST $OPENAPP_HOST_URL/<method> (or /<module>/<method> for host modules) with Authorization: Bearer $OPENAPP_HOST_TOKEN and JSON params; env is set for every module server",
            "from_views": "{ \"host\": \"host.call\", \"method\": \"…\", \"params\": {…}, \"module\"? } (iframe: openapp.host(\"host.call\", {...}) resolves with the result); dictation: { \"host\": \"audio.record\", \"transcribe\": \"apple\", \"insert\": true }",
            "host_modules": modules,
            "host_modules_howto": "need more of macOS? declare \"host\": { \"swift\": \"host/main.swift\", \"summary\": \"one sentence\" } in openapp.json; the kernel compiles it on the Mac after the user allows it (see the builder skill)"
        })
    }

    pub fn describe(&self) -> Value {
        let host_services = self.host_services_info();
        let vm = self.vm();
        let g = self.lock();
        let (_, slots) = view_index(&g);
        let modules: Vec<Value> = g
            .modules
            .values()
            .map(|m| {
                let views: Map<String, Value> = m
                    .views
                    .iter()
                    .map(|(n, v)| {
                        let mut o = json!({ "kind": v.kind, "file": v.file });
                        if let Some(e) = v.value.get("error") {
                            o["error"] = e.clone();
                        }
                        if !v.problems.is_empty() {
                            o["problems"] = json!(v.problems);
                        }
                        (n.clone(), o)
                    })
                    .collect();
                json!({
                    "id": m.id,
                    "name": m.name,
                    "status": m.status,
                    "error": m.error,
                    "views": views,
                    "tools": m.tools.iter().map(|t| json!({ "name": t.name, "description": t.description })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let in_use: BTreeSet<&str> = g.layout.placements.iter().map(|p| p.anchor.as_str()).collect();
        let mut available: Vec<String> = layout::FIXED_ANCHORS.iter().map(|s| s.to_string()).collect();
        available.extend(g.layout.regions.iter().map(|r| format!("region:{}", r.id)));
        available.extend(slots.iter().map(|s| format!("slot:{s}")));
        let tools: Value = match &vm {
            Some(v) => v.guest_tools.locked().clone(),
            None => Value::Object(
                ["node", "deno", "bun", "python3", "cargo", "ffmpeg", "git"]
                    .iter()
                    .map(|t| {
                        (t.to_string(), modules::which(t, self.path_env()).map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Bool(false)))
                    })
                    .collect(),
            ),
        };
        let vm_info = match &vm {
            Some(v) => json!({
                "running": true,
                "os": "linux",
                "workspace": crate::vm::GUEST_WS,
                "note": "You run in a Linux VM. /workspace is the user's app folder (shared with the host); edits made on the host can take ~5 s to appear in /workspace. Network goes through OpenApp's proxy: package registries and your provider are allowed, other hosts ask the user. HTTP(S) uses HTTP(S)_PROXY; other TCP protocols (IMAP, MTProto, MQTT) use the SOCKS5 proxy in $OPENAPP_SOCKS_URL (pass it as the library's proxy option; socks5h, names resolved on the Mac), or \"net\": \"socks\" on a service/server runs it under proxychains4.",
                "socks_proxy": crate::vm::socks_url("agent"),
                "name": v.name,
            }),
            None => json!({ "running": false, "error": *self.vm_error.locked() }),
        };
        let mut d = json!({
            "contract": CONTRACT_VERSION,
            "workspace": self.workspace.to_string_lossy(),
            "guide": "AGENTS.md in the workspace root",
            "layout": g.layout,
            "theme": theme::resolve(&g.theme),
            "theme_overrides": g.theme,
            "theme_tokens": { "colors": theme::COLOR_KEYS, "other": theme::OTHER_KEYS, "min_contrast": theme::MIN_CONTRAST },
            "modules": modules,
            "anchors": { "in_use": in_use, "available": available },
            "spec_catalog": views::catalog_summary(),
            "agent": g.agent,
            "sessions": g.session_metas(),
            "activeSession": g.active,
            "host_data": {
                "usage": "In a spec view any prop may be { \"$host\": \"<source>\" }; the shell fills it from the live snapshot.",
                "sources": {
                    "sessions": "list items [{ id, title, subtitle (relative time), icon: \"message-square\", active, action: {host:\"session.open\", id} }] — use as list.items",
                    "harnesses": "list items [{ id, title: name, subtitle: note, icon: \"bot\", active, action: {host:\"harness.select\", id} }] — the provider picker",
                    "grants": "list items [{ id, title: host or module, subtitle: kind + decision, icon, action: {host:\"grant.revoke\", id} }] — what the user allowed (network hosts, web control, microphone); build a grants panel with it",
                    "secrets": "list items [{ id: name, title: label, subtitle: kind · provider, icon: \"key-round\", action: {host:\"secret.delete\", name} }] — the user's connected accounts and keys (metadata only, never values)",
                    "session.active.title": "string",
                    "agent.name": "string",
                    "agent.status": "string (starting|ready|working|error)"
                }
            },
            "host_actions": crate::primitives::builtin_docs(),
            "harnesses": *self.harnesses.locked(),
            "capabilities": capabilities(self.web_host.get().is_some(), vm.is_some()),
            "host_services": host_services,
            "placement_rules": crate::workspace::placement_rules(),
            "presented": g.presented,
            "vm": vm_info,
            "kernel_gaps": KERNEL_GAPS,
            "undo_depth": g.history.len(),
            "host": {
                "tools_where": if vm.is_some() { "app VM (where your commands and module servers run)" } else { "host" },
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
                "memory_bytes": self.memory_bytes,
                "memory_gb": self.memory_bytes.map(|b| (b as f64 / 1_073_741_824.0 * 10.0).round() / 10.0),
                "tools": tools,
            }
        });
        drop(g);
        crate::primitives::extend_describe(self, &mut d);
        d
    }

    pub fn snapshot_text(&self) -> (String, Vec<Value>, Vec<String>) {
        // §21 G1: spec views show the current value of their `$stream` bindings
        let _streams = crate::primitives::channels::SnapshotStreams::enter(self);
        let native = crate::primitives::shell::summary_text(self);
        let g = self.lock();
        let mut out = String::new();
        let resolved = theme::resolve(&g.theme);
        let tv = |k: &str| resolved.get(k).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
        out.push_str(&format!(
            "OpenApp — what the user sees now\nagent: {} ({}) · theme: bg {} · surface {} · text {} · accent {} · radius {} · density {} ({} override(s))\n\n",
            g.agent.name,
            g.agent.status,
            tv("bg"),
            tv("surface"),
            tv("text"),
            tv("accent"),
            tv("radius"),
            tv("density"),
            g.theme.len()
        ));
        let mut rendered: BTreeSet<String> = BTreeSet::new();
        let edges = |e: &str| g.layout.regions.iter().filter(|r| r.edge == e).cloned().collect::<Vec<_>>();
        let section = |out: &mut String, title: String, anchor: String, rendered: &mut BTreeSet<String>| {
            out.push_str(&format!("[{title}]\n"));
            render_anchor(&g, &anchor, 1, out, rendered, 0);
        };
        for e in ["top", "left"] {
            for r in edges(e) {
                section(&mut out, region_title(&r), format!("region:{}", r.id), &mut rendered);
            }
        }
        section(&mut out, "main (main column, above the chat)".into(), "main".into(), &mut rendered);
        out.push_str(&format!(
            "[chat] session {} \"{}\": {} item(s) ({} session(s) in total)\n",
            g.active,
            g.sessions.get(&g.active).map(|s| s.rec.title.as_str()).unwrap_or(""),
            g.active_chat().len(),
            g.sessions.len()
        ));
        section(&mut out, "composer.above (strip above the input)".into(), "composer.above".into(), &mut rendered);
        out.push_str("[composer] text input + Send\n");
        section(&mut out, "composer.actions (buttons left of Send)".into(), "composer.actions".into(), &mut rendered);
        for e in ["right", "bottom"] {
            for r in edges(e) {
                section(&mut out, region_title(&r), format!("region:{}", r.id), &mut rendered);
            }
        }
        let orphans: Vec<_> = g.layout.placements.iter().filter(|p| !rendered.contains(&p.id)).collect();
        if !orphans.is_empty() {
            out.push_str("\n[not visible] placements whose anchor is not on screen:\n");
            for p in orphans {
                out.push_str(&format!("  - {} {}/{} at {}\n", p.id, p.module, p.view, p.anchor));
            }
        }
        out.push_str("\nModules:\n");
        if g.modules.is_empty() {
            out.push_str("  (none yet)\n");
        }
        for m in g.modules.values() {
            out.push_str(&format!(
                "  - {} \"{}\": {}{}; tools: {}; views: {}\n",
                m.id,
                m.name,
                m.status,
                m.error.as_ref().map(|e| format!(" — {}", e.lines().next().unwrap_or(""))).unwrap_or_default(),
                if m.tools.is_empty() { "none".into() } else { m.tool_names().into_iter().collect::<Vec<_>>().join(", ") },
                if m.views.is_empty() { "none".into() } else { m.views.keys().cloned().collect::<Vec<_>>().join(", ") }
            ));
        }
        out.push_str(&native);
        let mut problems = Vec::new();
        for m in g.modules.values() {
            for (vn, v) in &m.views {
                if let Some(e) = v.value.get("error").and_then(Value::as_str) {
                    problems.push(format!("{}/{vn}: {e}", m.id));
                }
                problems.extend(v.problems.iter().cloned());
            }
        }
        if !problems.is_empty() {
            out.push_str("\nView problems:\n");
            for p in &problems {
                out.push_str(&format!("  - {p}\n"));
            }
        }
        let errs: Vec<Value> = g.ui_errors.iter().rev().take(20).rev().cloned().collect();
        out.push_str(&format!("\nUI errors (last {}):\n", errs.len()));
        if errs.is_empty() {
            out.push_str("  (none)\n");
        }
        for e in &errs {
            out.push_str(&format!(
                "  - {} [{}{}] {}\n",
                e["ts"].as_str().unwrap_or(""),
                e["placement"].as_str().unwrap_or(""),
                e["module"].as_str().map(|m| format!(" {m}")).unwrap_or_default(),
                e["message"].as_str().unwrap_or("")
            ));
        }
        (out, errs, problems)
    }
}
