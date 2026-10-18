//! Kernel chrome host actions: composer, prompt, links, toasts, sessions, providers, grants, host calls,
//! presentations, turn undo and the chat/build mode. Built-ins (see [`super::Builtin`]): they run before the
//! registry, synchronously; long work is spawned and its errors become toasts.

use super::{Builtin, Caller, src_params, src_str as get};
use crate::kernel::Kernel;
use serde_json::{Value, json};
use std::sync::Arc;

/// Spawn `f`; an error becomes a toast.
fn background<F>(k: &Arc<Kernel>, f: F)
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let k = k.clone();
    tokio::spawn(async move {
        if let Err(e) = f.await {
            k.notify(&e);
        }
    });
}

pub const ACTIONS: &[Builtin] = &[
    Builtin {
        name: "composer.insert",
        args: "{ text }",
        keys: &["text"],
        run: |k, src| {
            let text = get(src, "text").ok_or("composer.insert needs \"text\"")?;
            k.sink.emit("oa://composer", json!({ "insert": text }));
            Ok(())
        },
    },
    Builtin {
        name: "prompt",
        args: "{ text } — sends a message to the agent as the user",
        keys: &["text"],
        run: |k, src| {
            let text = get(src, "text").ok_or("prompt needs \"text\"")?;
            k.send_prompt(text);
            Ok(())
        },
    },
    Builtin {
        name: "open_url",
        args: "{ url }",
        keys: &["url"],
        run: |_, src| crate::kernel::open_url(&get(src, "url").ok_or("open_url needs \"url\"")?),
    },
    Builtin {
        name: "notify",
        args: "{ text } — toast",
        keys: &["text"],
        run: |k, src| {
            if src.iter().any(|v| super::shell_notify::is_system(v)) {
                // §21 G4: a system notification (title/body/actions) instead of a toast
                let (k2, params) = (k.clone(), Value::Object(src_params(src)));
                background(k, async move {
                    match super::call_action(&k2, "notify.post", params, Caller::new("user")).await {
                        Some(Err(e)) => Err(e),
                        _ => Ok(()),
                    }
                });
                return Ok(());
            }
            k.notify(&get(src, "text").ok_or("notify needs \"text\"")?);
            Ok(())
        },
    },
    Builtin {
        name: "session.new",
        args: "{} — new conversation (kernel-owned)",
        keys: &[],
        run: |k, _| {
            k.session_new();
            Ok(())
        },
    },
    Builtin {
        name: "session.open",
        args: "{ id }",
        keys: &["id"],
        run: |k, src| k.session_open(&get(src, "id").ok_or("session.open needs \"id\"")?),
    },
    Builtin {
        name: "session.rename",
        args: "{ id, title }",
        keys: &["id", "title"],
        run: |k, src| {
            let id = get(src, "id").ok_or("session.rename needs \"id\"")?;
            let title = get(src, "title").ok_or("session.rename needs \"title\"")?;
            k.session_rename(&id, &title)
        },
    },
    Builtin {
        name: "harness.select",
        args: "{ id } — switch the agent provider (kernel stops the old harness, starts the new one)",
        keys: &["id"],
        run: |k, src| crate::harness::select(k, &get(src, "id").ok_or("harness.select needs \"id\"")?),
    },
    Builtin {
        name: "harness.refresh",
        args: "{} — re-detect installed providers",
        keys: &[],
        run: |k, _| {
            let k = k.clone();
            tokio::spawn(async move { crate::harness::refresh(&k).await });
            Ok(())
        },
    },
    Builtin {
        name: "harness.login",
        args: "{ id, method? } — sign the provider in (VM providers: browser login; Codex method \"device\" = a device code; Pi method \"chatgpt\" | \"claude\")",
        keys: &[],
        run: |k, src| {
            let id = get(src, "id").ok_or("harness.login needs \"id\"")?;
            crate::harness::login(k, &id, get(src, "method").as_deref())
        },
    },
    Builtin {
        name: "grant.revoke",
        args: "{ id } — remove a grant (ids from host data `grants`)",
        keys: &[],
        run: |k, src| k.grant_revoke(&get(src, "id").ok_or("grant.revoke needs \"id\" (from host data `grants`)")?),
    },
    Builtin {
        name: "host.call",
        args: "{ method, params?, module? } — macOS host service / host module method",
        keys: &[],
        run: |k, src| {
            let method = get(src, "method").ok_or("host.call needs \"method\"")?;
            let (k2, module) = (k.clone(), get(src, "module"));
            let params = src.iter().find_map(|v| v.get("params").cloned()).unwrap_or_else(|| json!({}));
            background(k, async move { k2.host_call(module.as_deref(), &method, params, "user").await.map(|_| ()).map_err(String::from) });
            Ok(())
        },
    },
    Builtin {
        name: "view.open",
        args: "{ view: \"<module>/<view>\", as: sheet|modal|inline|tab, title?, width?, region? } — present a view (kernel chrome)",
        keys: &[],
        run: |k, src| k.view_open(src).map(|_| ()),
    },
    Builtin {
        name: "view.close",
        args: "{ view? } — close the top presentation",
        keys: &[],
        run: |k, src| k.view_close(get(src, "view").as_deref()).map(|_| ()),
    },
    Builtin {
        name: "turn.undo",
        args: "{ turnId? } — revert a build turn (layout/theme changes, modules first loaded in it)",
        keys: &[],
        run: |k, src| {
            let (k2, t) = (k.clone(), get(src, "turnId"));
            background(k, async move { k2.turn_undo(t.as_deref()).await.map(|_| ()) });
            Ok(())
        },
    },
    Builtin {
        name: "mode.set",
        args: "",
        keys: &["mode"],
        run: |k, src| {
            let mode = get(src, "mode").ok_or("mode.set needs \"mode\" (chat or build)")?;
            k.session_set_mode(&k.active_session(), &mode)
        },
    },
];
