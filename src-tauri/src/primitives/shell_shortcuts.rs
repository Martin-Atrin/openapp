//! §21 G4 shortcuts: `shortcut.register { accelerator, action, global? }`.
//! App-wide (Free): the shell matches key presses in every OpenApp window (and in html views, which forward keys)
//! and fires `shortcut.fire`. Global (Granted, one card per module): the global-shortcut plugin, works while
//! OpenApp is in the background. Conflicts (kernel menu keys, other modules, other apps) are reported.

use crate::research::Kind;
use crate::kernel::HostError;
use crate::util::MutexExt;
use super::shell::{self, s, st};
use super::*;

pub fn primitive() -> Primitive {
    let reg = handler(|k, p, c| async move { register(&k, &p, &c).await });
    let unreg = handler(|k, p, c| async move { unregister(&k, &p, &c) });
    Primitive {
        name: "shortcut",
        purpose: "Keyboard shortcuts: app-wide (any OpenApp window) or global (also in the background, needs the user's OK)",
        example: r#"{ "host": "shortcut.register", "accelerator": "Space", "action": { "tool": "play_pause" } }"#,
        class: Class::Free,
        actions: vec![
            Action { name: "shortcut.register", args: "{ accelerator: \"Cmd+Shift+K\" | \"Space\" | \"MediaPlayPause\", action, global?, id?, inInputs? } → { id, accelerator, conflicts }", handler: reg.clone() },
            Action { name: "shortcut.unregister", args: "{ id | accelerator }", handler: unreg.clone() },
        ],
        endpoints: vec![
            Endpoint { method: "shortcut.register", args: "{ accelerator, action, global?, id? }", handler: reg },
            Endpoint { method: "shortcut.unregister", args: "{ id | accelerator }", handler: unreg },
        ],
        data: vec![DataSource {
            name: "shortcuts",
            doc: "registered shortcuts: [{ id, module, accelerator, global, code, mods }]",
            get: data_fn(|k| Value::Array(st(k).shortcuts.locked().values().map(|v| {
                let mut v = v.clone();
                if let Some(o) = v.as_object_mut() { o.remove("action"); }
                v
            }).collect())),
        }],
        commands: vec![("shortcut.fire", handler(|k, p, c| async move { fire(&k, &p, &c).await }))],
        ..Default::default()
    }
}

/// A parsed accelerator. Modifiers in canonical order Cmd, Ctrl, Alt, Shift; `key` canonical name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accel {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

const NAMED: &[(&str, &[&str], &str)] = &[
    // canonical, aliases (lower-case), KeyboardEvent.code
    ("Space", &["space", " "], "Space"),
    ("Enter", &["enter", "return"], "Enter"),
    ("Escape", &["escape", "esc"], "Escape"),
    ("Tab", &["tab"], "Tab"),
    ("Backspace", &["backspace"], "Backspace"),
    ("Delete", &["delete", "del"], "Delete"),
    ("Up", &["up", "arrowup"], "ArrowUp"),
    ("Down", &["down", "arrowdown"], "ArrowDown"),
    ("Left", &["left", "arrowleft"], "ArrowLeft"),
    ("Right", &["right", "arrowright"], "ArrowRight"),
    ("Home", &["home"], "Home"),
    ("End", &["end"], "End"),
    ("PageUp", &["pageup"], "PageUp"),
    ("PageDown", &["pagedown"], "PageDown"),
    ("Comma", &[",", "comma"], "Comma"),
    ("Period", &[".", "period"], "Period"),
    ("Slash", &["/", "slash"], "Slash"),
    ("Semicolon", &[";", "semicolon"], "Semicolon"),
    ("Quote", &["'", "quote"], "Quote"),
    ("BracketLeft", &["[", "bracketleft"], "BracketLeft"),
    ("BracketRight", &["]", "bracketright"], "BracketRight"),
    ("Backslash", &["\\", "backslash"], "Backslash"),
    ("Minus", &["-", "minus"], "Minus"),
    ("Equal", &["=", "equal", "plus"], "Equal"),
    ("Backquote", &["`", "backquote"], "Backquote"),
    ("MediaPlayPause", &["mediaplaypause", "playpause"], "MediaPlayPause"),
    ("MediaTrackNext", &["mediatracknext", "medianexttrack", "nexttrack"], "MediaTrackNext"),
    ("MediaTrackPrevious", &["mediatrackprevious", "mediaprevtrack", "prevtrack"], "MediaTrackPrevious"),
    ("MediaStop", &["mediastop"], "MediaStop"),
];

impl Accel {
    pub fn parse(s: &str) -> Result<Accel, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty accelerator".into());
        }
        // "+" alone or as the last key ("Cmd++")
        let mut parts: Vec<String> = s.split('+').map(|p| p.trim().to_string()).collect();
        if s.ends_with("++") || s == "+" {
            parts.retain(|p| !p.is_empty());
            parts.push("Equal".into());
        }
        let (key, mods) = parts.split_last().ok_or("empty accelerator")?;
        let mut a = Accel { cmd: false, ctrl: false, alt: false, shift: false, key: String::new() };
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "meta" | "super" | "cmdorctrl" | "commandorcontrol" | "mod" | "cmdorcontrol" | "commandorctrl" => a.cmd = true,
                "ctrl" | "control" => a.ctrl = true,
                "alt" | "option" | "opt" => a.alt = true,
                "shift" => a.shift = true,
                other => return Err(format!("unknown modifier \"{other}\" in \"{s}\" (use Cmd, Ctrl, Alt, Shift)")),
            }
        }
        let k = key.trim();
        let lower = k.to_ascii_lowercase();
        a.key = if k.chars().count() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) {
            k.to_ascii_uppercase()
        } else if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=24).contains(n)) {
            format!("F{n}")
        } else if let Some(d) = lower.strip_prefix("digit").filter(|d| d.len() == 1 && d.chars().all(|c| c.is_ascii_digit())) {
            d.to_string()
        } else if let Some(c) = lower.strip_prefix("key").filter(|c| c.len() == 1 && c.chars().all(|c| c.is_ascii_alphabetic())) {
            c.to_ascii_uppercase()
        } else if let Some((canon, _, _)) = NAMED.iter().find(|(c, al, _)| c.eq_ignore_ascii_case(k) || al.contains(&lower.as_str()) || al.contains(&k)) {
            canon.to_string()
        } else {
            return Err(format!("unknown key \"{k}\" in \"{s}\" (letters, digits, F1–F24, Space, Enter, Escape, Tab, Up/Down/Left/Right, punctuation, MediaPlayPause…)"));
        };
        Ok(a)
    }

    /// `Cmd+Shift+K` (accepted by the menu and the global-shortcut parsers).
    pub fn canonical(&self) -> String {
        let mut v = Vec::new();
        if self.cmd {
            v.push("Cmd");
        }
        if self.ctrl {
            v.push("Ctrl");
        }
        if self.alt {
            v.push("Alt");
        }
        if self.shift {
            v.push("Shift");
        }
        v.push(&self.key);
        v.join("+")
    }

    /// `KeyboardEvent.code` of the key (the shell matches on code, so Alt+letter works on macOS).
    pub fn code(&self) -> String {
        let k = self.key.as_str();
        if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphabetic()) {
            format!("Key{k}")
        } else if k.len() == 1 && k.chars().all(|c| c.is_ascii_digit()) {
            format!("Digit{k}")
        } else if k.starts_with('F') && k[1..].parse::<u8>().is_ok() {
            k.to_string()
        } else {
            NAMED.iter().find(|(c, _, _)| *c == k).map(|(_, _, code)| code.to_string()).unwrap_or_else(|| k.to_string())
        }
    }

    pub fn is_media(&self) -> bool {
        self.key.starts_with("Media")
    }

    /// For the global-shortcut plugin (`Command+Shift+KeyK` style it parses too, but it accepts this form).
    pub fn global_str(&self) -> String {
        let mut v: Vec<String> = Vec::new();
        if self.cmd {
            v.push("Command".into());
        }
        if self.ctrl {
            v.push("Control".into());
        }
        if self.alt {
            v.push("Alt".into());
        }
        if self.shift {
            v.push("Shift".into());
        }
        v.push(self.code());
        v.join("+")
    }
}

async fn register(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, HostError> {
    let module = shell::owner(k, caller, p)?;
    let accel = Accel::parse(s(p, "accelerator").ok_or("shortcut.register needs \"accelerator\" (e.g. \"Cmd+Shift+K\" or \"Space\")")?)?;
    let action = p.get("action").filter(|a| a.is_object()).cloned().ok_or("shortcut.register needs \"action\" ({tool, args} | {host, …} | {channel, data})")?;
    let global = p.get("global").and_then(Value::as_bool).unwrap_or(false);
    if accel.is_media() && !global {
        return Err(HostError::bad_request(format!("{} works only as a global shortcut (\"global\": true)", accel.key)));
    }
    let canon = accel.canonical();
    if super::shell_menu::KERNEL_ACCELS.contains(&canon.as_str()) {
        return Err(HostError::conflict(format!("{canon} belongs to OpenApp's own menu (conflict); pick another accelerator")));
    }
    let id = s(p, "id").map(String::from).unwrap_or_else(|| format!("{module}:{canon}"));
    let mut conflicts = Vec::new();
    {
        let s = st(k);
        let sc = s.shortcuts.locked();
        if let Some((oid, o)) = sc.iter().find(|(oid, o)| **oid != id && o["accelerator"] == canon.as_str() && o["global"] == global) {
            if o["module"] != module.as_str() {
                return Err(HostError::conflict(format!("{canon} is already used by \"{}\" (shortcut {oid}); pick another accelerator", o["module"].as_str().unwrap_or("?"))));
            }
            conflicts.push(json!({ "accelerator": canon, "with": oid }));
        }
        let menus = s.menus.locked();
        for (m, v) in menus.iter() {
            for (a, label) in super::shell_menu::validate(v).1 {
                if a == canon {
                    conflicts.push(json!({ "accelerator": canon, "with": format!("menu item \"{label}\" of {m}") }));
                }
            }
        }
    }
    if global {
        let name = k.lock().modules.get(&module).map(|m| m.name.clone()).unwrap_or_else(|| module.clone());
        shell::consent(k, "shortcut.global", &module, &format!("{name} wants a global shortcut ({canon})"), "Global shortcuts work while OpenApp is in the background. The module learns only that the key combination was pressed.").await?;
    }
    let entry = json!({
        "id": id, "module": module, "accelerator": canon, "global": global, "action": action,
        "code": accel.code(), "mods": { "meta": accel.cmd, "ctrl": accel.ctrl, "alt": accel.alt, "shift": accel.shift },
        "inInputs": p.get("inInputs").and_then(Value::as_bool).unwrap_or(accel.cmd || accel.ctrl),
    });
    let prev = st(k).shortcuts.locked().insert(id.clone(), entry.clone());
    if let Some(prev) = prev.filter(|p| p["global"] == true) {
        unregister_global(k, &prev);
    }
    // no app window: stored, registered by `restore` when the window opens
    if global && shell::attached(k) && let Err(e) = register_global(k, &entry) {
        st(k).shortcuts.locked().remove(&id);
        return Err(HostError::upstream(format!("could not register the global shortcut {canon}: {e} (another app may use it)")));
    }
    shell::save(k);
    k.research.log(&caller.id, Kind::Shortcut, json!({ "op": "register", "module": module, "accelerator": canon, "global": global }));
    k.emit_snapshot();
    let mut out = json!({ "ok": true, "id": id, "accelerator": canon, "global": global, "conflicts": conflicts });
    shell::applied_out(k, &mut out, if shell::attached(k) { Ok(()) } else { Err(String::new()) });
    Ok(out)
}

fn unregister(k: &Arc<Kernel>, p: &Value, caller: &Caller) -> Result<Value, String> {
    let module = shell::owner(k, caller, p)?;
    let target_id = match (s(p, "id"), s(p, "accelerator")) {
        (Some(i), _) => i.to_string(),
        (None, Some(a)) => format!("{module}:{}", Accel::parse(a)?.canonical()),
        _ => return Err("shortcut.unregister needs \"id\" or \"accelerator\"".into()),
    };
    let removed = {
        let s = st(k);
        let mut sc = s.shortcuts.locked();
        match sc.get(&target_id) {
            Some(e) if e["module"] == module.as_str() => sc.remove(&target_id),
            Some(_) => return Err(format!("shortcut \"{target_id}\" belongs to another module")),
            None => None,
        }
    };
    let e = removed.ok_or_else(|| format!("no shortcut \"{target_id}\""))?;
    if e["global"] == true {
        unregister_global(k, &e);
    }
    shell::save(k);
    k.emit_snapshot();
    Ok(json!({ "ok": true, "id": target_id }))
}

/// Shell command: an app-wide shortcut was pressed (only the shell can send it).
async fn fire(k: &Arc<Kernel>, p: &Value, _caller: &Caller) -> Result<Value, String> {
    let id = s(p, "id").ok_or("shortcut.fire needs \"id\"")?;
    let e = st(k).shortcuts.locked().get(id).cloned().ok_or_else(|| format!("no shortcut \"{id}\""))?;
    let module = e["module"].as_str().unwrap_or("").to_string();
    shell::run_action(k, &format!("shortcut:{id}"), &module, &e["action"], json!({ "accelerator": e["accelerator"] })).await
}

fn global_shortcut(e: &Value) -> Option<tauri_plugin_global_shortcut::Shortcut> {
    let a = Accel::parse(e["accelerator"].as_str()?).ok()?;
    a.global_str().parse().ok()
}

pub fn register_global(k: &Arc<Kernel>, e: &Value) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let app = shell::app(k)?;
    let sc = global_shortcut(e).ok_or("bad accelerator")?;
    let weak = Arc::downgrade(k);
    let id = e["id"].as_str().unwrap_or("").to_string();
    app.global_shortcut()
        .on_shortcut(sc, move |_app, _sc, ev| {
            if ev.state != ShortcutState::Pressed {
                return;
            }
            let Some(k) = weak.upgrade() else { return };
            let id = id.clone();
            k.clone().spawn(async move {
                let _ = fire(&k, &json!({ "id": id }), &Caller::new("user")).await;
            });
        })
        .map_err(|e| e.to_string())
}

pub fn unregister_global(k: &Kernel, e: &Value) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    if let (Ok(app), Some(sc)) = (shell::app(k), global_shortcut(e)) {
        let _ = app.global_shortcut().unregister(sc);
    }
}

/// Re-register the saved global shortcuts (start), skipping modules whose grant was revoked.
pub fn restore(k: &Arc<Kernel>) {
    let list: Vec<Value> = st(k).shortcuts.locked().values().filter(|v| v["global"] == true).cloned().collect();
    for e in list {
        let m = e["module"].as_str().unwrap_or("");
        if shell::grant(k, "shortcut.global", m) == Some(true)
            && let Err(err) = register_global(k, &e)
        {
            log::warn!("global shortcut {} not restored: {err}", e["accelerator"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accelerators() {
        let a = Accel::parse("CmdOrCtrl+Shift+k").unwrap();
        assert_eq!(a.canonical(), "Cmd+Shift+K");
        assert_eq!(a.code(), "KeyK");
        assert_eq!(Accel::parse("space").unwrap().canonical(), "Space");
        assert_eq!(Accel::parse("Alt+ArrowUp").unwrap().code(), "ArrowUp");
        assert_eq!(Accel::parse("ctrl+alt+shift+cmd+1").unwrap().canonical(), "Cmd+Ctrl+Alt+Shift+1");
        assert_eq!(Accel::parse("F12").unwrap().code(), "F12");
        assert_eq!(Accel::parse("Cmd+,").unwrap().canonical(), "Cmd+Comma");
        assert!(Accel::parse("Hyper+K").is_err());
        assert!(Accel::parse("Cmd+Banana").is_err());
        assert!(Accel::parse("MediaPlayPause").unwrap().is_media());
        // the global-shortcut plugin parses our form
        for s in ["Cmd+Shift+K", "Alt+Space", "Ctrl+F5", "MediaPlayPause", "Cmd+Comma", "Shift+Up"] {
            let g = Accel::parse(s).unwrap().global_str();
            assert!(g.parse::<tauri_plugin_global_shortcut::Shortcut>().is_ok(), "{g}");
        }
    }
}
