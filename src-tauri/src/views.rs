//! Spec view catalog: validation, slot discovery and text rendering (contract §4.1).

use serde_json::{Value, json};
use std::collections::BTreeSet;

/// (type, required props, optional props)
pub const CATALOG: &[(&str, &[&str], &[&str])] = &[
    ("stack", &[], &["direction", "gap", "align", "justify", "padding", "children"]),
    ("text", &["value"], &["variant"]),
    ("button", &["label", "action"], &["icon", "variant"]),
    ("icon-button", &["icon", "label", "action"], &["active"]),
    ("input", &["name"], &["placeholder", "value", "submit"]),
    ("toggle", &["label", "value", "action"], &[]),
    ("list", &["items"], &["empty"]),
    ("card", &[], &["title", "children"]),
    ("badge", &["label"], &["tone"]),
    ("progress", &["value"], &[]),
    ("image", &["src"], &["alt", "height"]),
    ("divider", &[], &[]),
    ("slot", &["id"], &[]),
    // §21 G4: drop files here; the action gets { files: [{ path, name, size, mime }] }
    ("dropzone", &["action"], &["label", "hint", "icon", "children"]),
];

const ENUMS: &[(&str, &str, &[&str])] = &[
    ("stack", "direction", &["column", "row"]),
    ("stack", "align", &["start", "center", "end", "stretch"]),
    ("stack", "justify", &["start", "center", "end", "between"]),
    ("text", "variant", &["body", "title", "heading", "muted", "mono", "small"]),
    ("button", "variant", &["primary", "secondary", "ghost", "danger"]),
    ("badge", "tone", &["neutral", "accent", "success", "warning", "danger"]),
];

/// Host data sources a spec prop can bind with `{ "$host": "<source>" }` (§10).
pub const HOST_SOURCES: &[&str] = &["sessions", "harnesses", "grants", "session.active.title", "agent.name", "agent.status"];

pub fn host_binding(v: &Value) -> Option<&str> {
    v.as_object().filter(|o| o.len() == 1).and_then(|o| o.get("$host")).and_then(Value::as_str)
}

pub fn catalog_summary() -> Value {
    let types: Vec<Value> = CATALOG
        .iter()
        .map(|(t, req, opt)| json!({ "type": t, "required": req, "optional": opt }))
        .collect();
    json!({
        "node": "{ \"type\": string, ...props, \"children\"?: Node[] }",
        "types": types,
        "enums": ENUMS.iter().map(|(t, p, v)| json!({"type": t, "prop": p, "values": v})).collect::<Vec<_>>(),
        "action": [
            "{ \"tool\": \"name\", \"args\": {...} }  -> calls the tool on the view's own module server",
            "{ \"host\": \"composer.insert\", \"text\": \"...\" }",
            "{ \"host\": \"prompt\", \"text\": \"...\" }  -> sends a message to the agent as the user",
            "{ \"host\": \"open_url\", \"url\": \"https://...\" }"
        ],
        "notes": [
            "input.submit and toggle.action get { value } merged into args",
            "icons are lucide names in kebab-case (mic, square, folder-open, github)",
            "unknown types render as a red error box",
            "any prop may be { \"$host\": \"<source>\" } (sources: sessions, harnesses, grants, session.active.title, agent.name, agent.status, web:<placement>); e.g. {\"type\":\"list\",\"items\":{\"$host\":\"sessions\"}}",
            "list items accept active?: boolean (highlighted)",
            "tool results may return structuredContent.openapp = { composer_insert?, prompt?, notify? }"
        ]
    })
}

fn check_action(a: &Value, path: &str, tools: Option<&BTreeSet<String>>, out: &mut Vec<String>) {
    let Some(o) = a.as_object() else {
        out.push(format!("{path}: action must be an object like {{\"tool\":\"name\",\"args\":{{}}}}"));
        return;
    };
    if o.get("host").and_then(Value::as_str) == Some("audio.record") && !o.get("tool").is_some_and(Value::is_string) {
        out.push(format!("{path}: host action \"audio.record\" needs a string \"tool\" (the module tool that gets the recording)"));
        return;
    }
    if let Some(t) = o.get("tool") {
        match t.as_str() {
            Some(name) => {
                if let Some(tools) = tools
                    && !tools.contains(name) {
                        out.push(format!(
                            "{path}: tool \"{name}\" is not exposed by this module's server (tools: {})",
                            if tools.is_empty() { "none".into() } else { tools.iter().cloned().collect::<Vec<_>>().join(", ") }
                        ));
                    }
            }
            None => out.push(format!("{path}: \"tool\" must be a string")),
        }
        if let Some(args) = o.get("args")
            && !args.is_object() {
                out.push(format!("{path}: \"args\" must be an object"));
            }
    } else if let Some(h) = o.get("host") {
        match h.as_str() {
            Some(h) if crate::primitives::builtin(h).is_some() => {
                for key in crate::primitives::builtin(h).map(|b| b.keys).unwrap_or_default() {
                    if !o.get(*key).is_some_and(Value::is_string) {
                        out.push(format!("{path}: host action \"{h}\" needs a string \"{key}\""));
                    }
                }
            }
            // §21 registry host actions (window.open, menu.set, files.save, …)
            Some(h) if crate::primitives::action(h).is_some() => {}
            _ => out.push(format!("{path}: unknown host action {h} (use {})", crate::primitives::host_actions().join(", "))),
        }
    } else {
        out.push(format!("{path}: action needs \"tool\" or \"host\""));
    }
}

/// Validate a spec tree. Returns human-readable problems (empty = fine).
pub fn validate(tree: &Value, tools: Option<&BTreeSet<String>>) -> Vec<String> {
    let mut out = Vec::new();
    walk_validate(tree, "$", tools, &mut out, 0);
    out
}

fn walk_validate(n: &Value, path: &str, tools: Option<&BTreeSet<String>>, out: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        out.push(format!("{path}: tree is nested too deeply"));
        return;
    }
    let Some(o) = n.as_object() else {
        out.push(format!("{path}: node must be an object with a \"type\""));
        return;
    };
    let Some(ty) = o.get("type").and_then(Value::as_str) else {
        out.push(format!("{path}: node has no string \"type\""));
        return;
    };
    let Some((_, req, _)) = CATALOG.iter().find(|(t, _, _)| *t == ty) else {
        let known: Vec<&str> = CATALOG.iter().map(|c| c.0).collect();
        out.push(format!(
            "{path}: unknown type \"{ty}\" (renders as a red error box; known: {})",
            known.join(", ")
        ));
        return;
    };
    for (k, v) in o {
        if v.as_object().is_some_and(|m| m.contains_key("$host")) {
            match host_binding(v) {
                Some(src) if HOST_SOURCES.contains(&src) || crate::primitives::is_data_source(src) || src.strip_prefix("web:").is_some_and(|p| !p.is_empty()) => {}
                _ => out.push(format!(
                    "{path}.{k}: unknown host data {v} (use {{\"$host\": \"<source>\"}} with one of {})",
                    HOST_SOURCES.join(", ")
                )),
            }
        }
        // §21 G1: { "$stream": "<owner>/<name>", "path"?: "a.b", "default"? }
        if let Some(t) = v.as_object().and_then(|m| m.get("$stream")) {
            match t.as_str().map(crate::primitives::channels::parse_topic) {
                Some(Ok(_)) => {}
                Some(Err(e)) => out.push(format!("{path}.{k}: $stream: {e}")),
                None => out.push(format!("{path}.{k}: $stream must be a topic string")),
            }
        }
    }
    let bound = |p: &str| o.get(p).is_some_and(|v| host_binding(v).is_some() || v.get("$stream").is_some());
    for r in *req {
        if !o.contains_key(*r) {
            out.push(format!("{path}: {ty} needs \"{r}\""));
        }
    }
    for (t, p, vals) in ENUMS {
        if *t == ty
            && let Some(v) = o.get(*p).filter(|_| !bound(p))
                && !v.as_str().is_some_and(|s| vals.contains(&s)) {
                    out.push(format!("{path}: {ty}.{p} = {v} must be one of {}", vals.join(", ")));
                }
    }
    match ty {
        "button" | "icon-button" | "toggle" | "dropzone" => {
            if let Some(a) = o.get("action") {
                check_action(a, &format!("{path}.action"), tools, out);
            }
        }
        "input" => {
            if let Some(a) = o.get("submit") {
                check_action(a, &format!("{path}.submit"), tools, out);
            }
        }
        "list" if bound("items") => {}
        "list" => match o.get("items") {
            Some(Value::Array(items)) => {
                for (i, it) in items.iter().enumerate() {
                    let ip = format!("{path}.items[{i}]");
                    if it.get("id").is_none() || it.get("title").is_none() {
                        out.push(format!("{ip}: list items need \"id\" and \"title\""));
                    }
                    if let Some(a) = it.get("action") {
                        check_action(a, &format!("{ip}.action"), tools, out);
                    }
                }
            }
            Some(_) => out.push(format!("{path}: list.items must be an array")),
            None => {}
        },
        "progress" if bound("value") => {}
        "progress" => {
            if let Some(v) = o.get("value")
                && !v.as_f64().is_some_and(|x| (0.0..=1.0).contains(&x)) {
                    out.push(format!("{path}: progress.value must be a number 0..1"));
                }
        }
        "slot" => {
            if !o.get("id").and_then(Value::as_str).is_some_and(crate::layout::valid_ident) {
                out.push(format!("{path}: slot.id must match [A-Za-z0-9_-]+"));
            }
        }
        _ => {}
    }
    match o.get("children") {
        None | Some(Value::Null) => {}
        Some(Value::Array(ch)) => {
            for (i, c) in ch.iter().enumerate() {
                walk_validate(c, &format!("{path}.children[{i}]"), tools, out, depth + 1);
            }
        }
        Some(_) => out.push(format!("{path}: \"children\" must be an array")),
    }
}

pub fn collect_slots(tree: &Value, out: &mut BTreeSet<String>) {
    if let Some(o) = tree.as_object() {
        if o.get("type").and_then(Value::as_str) == Some("slot")
            && let Some(id) = o.get("id").and_then(Value::as_str) {
                out.insert(id.to_string());
            }
        if let Some(Value::Array(ch)) = o.get("children") {
            for c in ch {
                collect_slots(c, out);
            }
        }
    }
}

fn short(s: &str) -> String {
    crate::util::short(s, 80)
}

pub fn action_text(a: &Value) -> String {
    if let Some(t) = a.get("tool").and_then(Value::as_str) {
        match a.get("args") {
            Some(args) if args.as_object().is_some_and(|o| !o.is_empty()) => {
                format!("tool:{t} {}", short(&args.to_string()))
            }
            _ => format!("tool:{t}"),
        }
    } else if let Some(h) = a.get("host").and_then(Value::as_str) {
        let arg = a.get("text").or_else(|| a.get("url")).and_then(Value::as_str).unwrap_or("");
        format!("host:{h} \"{}\"", short(arg))
    } else {
        "invalid action".into()
    }
}

fn s<'a>(o: &'a serde_json::Map<String, Value>, k: &str) -> &'a str {
    o.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Render a spec tree as indented text. `slot` renders the placements in a slot.
pub fn render(tree: &Value, indent: usize, out: &mut String, slot: &mut dyn FnMut(&str, usize, &mut String)) {
    render_inner(tree, indent, out, slot, 0)
}

fn render_inner(
    n: &Value,
    indent: usize,
    out: &mut String,
    slot: &mut dyn FnMut(&str, usize, &mut String),
    depth: usize,
) {
    let pad = "  ".repeat(indent);
    if depth > 32 {
        out.push_str(&format!("{pad}… (too deep)\n"));
        return;
    }
    let Some(o) = n.as_object() else {
        out.push_str(&format!("{pad}!! invalid node {}\n", short(&n.to_string())));
        return;
    };
    let ty = s(o, "type");
    let line = match ty {
        "stack" => {
            let mut bits = vec![o.get("direction").and_then(Value::as_str).unwrap_or("column").to_string()];
            if let Some(g) = o.get("gap") {
                bits.push(format!("gap {g}"));
            }
            format!("stack({})", bits.join(", "))
        }
        "text" => {
            let v = o
                .get("value")
                .map(|v| match host_binding(v) {
                    Some(src) => format!("{{$host {src}}}"),
                    None => v.as_str().map(String::from).unwrap_or_else(|| v.to_string()),
                })
                .unwrap_or_default();
            match o.get("variant").and_then(Value::as_str) {
                Some(var) => format!("text[{var}] \"{}\"", short(&v)),
                None => format!("text \"{}\"", short(&v)),
            }
        }
        "button" => {
            let mut extra = Vec::new();
            if let Some(i) = o.get("icon").and_then(Value::as_str) {
                extra.push(format!("icon {i}"));
            }
            if let Some(v) = o.get("variant").and_then(Value::as_str) {
                extra.push(v.to_string());
            }
            format!(
                "button \"{}\"{} → {}",
                short(s(o, "label")),
                if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) },
                o.get("action").map(action_text).unwrap_or_else(|| "no action".into())
            )
        }
        "icon-button" => format!(
            "icon-button {} \"{}\"{} → {}",
            s(o, "icon"),
            short(s(o, "label")),
            if o.get("active").and_then(Value::as_bool) == Some(true) { " [active]" } else { "" },
            o.get("action").map(action_text).unwrap_or_else(|| "no action".into())
        ),
        "input" => {
            let mut l = format!("input name={}", s(o, "name"));
            if let Some(p) = o.get("placeholder").and_then(Value::as_str) {
                l.push_str(&format!(" placeholder=\"{}\"", short(p)));
            }
            if let Some(v) = o.get("value").and_then(Value::as_str) {
                l.push_str(&format!(" value=\"{}\"", short(v)));
            }
            if let Some(a) = o.get("submit") {
                l.push_str(&format!(" submit → {}", action_text(a)));
            }
            l
        }
        "toggle" => format!(
            "toggle \"{}\" = {} → {}",
            short(s(o, "label")),
            o.get("value").map(|v| v.to_string()).unwrap_or_default(),
            o.get("action").map(action_text).unwrap_or_else(|| "no action".into())
        ),
        "list" if o.get("items").and_then(host_binding).is_some() => {
            format!("list (host data: {})", o.get("items").and_then(host_binding).unwrap_or(""))
        }
        "list" => {
            let items = o.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            if items.is_empty() {
                format!("list (empty: \"{}\")", short(s(o, "empty")))
            } else {
                let mut l = format!("list ({} items)", items.len());
                for it in items.iter().take(30) {
                    let t = it.get("title").and_then(Value::as_str).unwrap_or("?");
                    let mut item = format!("\n{pad}  • {}", short(t));
                    if let Some(st) = it.get("subtitle").and_then(Value::as_str) {
                        item.push_str(&format!(" — {}", short(st)));
                    }
                    if let Some(a) = it.get("action") {
                        item.push_str(&format!(" → {}", action_text(a)));
                    }
                    l.push_str(&item);
                }
                if items.len() > 30 {
                    l.push_str(&format!("\n{pad}  … {} more", items.len() - 30));
                }
                l
            }
        }
        "card" => match o.get("title").and_then(Value::as_str) {
            Some(t) => format!("card \"{}\"", short(t)),
            None => "card".into(),
        },
        "badge" => format!(
            "badge \"{}\"{}",
            short(s(o, "label")),
            o.get("tone").and_then(Value::as_str).map(|t| format!(" ({t})")).unwrap_or_default()
        ),
        "progress" => format!(
            "progress {:.0}%",
            o.get("value").and_then(Value::as_f64).unwrap_or(0.0) * 100.0
        ),
        "image" => format!("image {} \"{}\"", short(s(o, "src")), short(s(o, "alt"))),
        "divider" => "divider".into(),
        "slot" => format!("slot \"{}\"", s(o, "id")),
        "dropzone" => format!("dropzone \"{}\"", short(o.get("label").and_then(Value::as_str).unwrap_or("Drop files here"))),
        other => format!("!! unknown type \"{other}\" (shown as a red error box)"),
    };
    out.push_str(&pad);
    out.push_str(&line);
    out.push('\n');
    if ty == "slot" {
        slot(s(o, "id"), indent + 1, out);
    }
    if let Some(Value::Array(ch)) = o.get("children") {
        for c in ch {
            render_inner(c, indent + 1, out, slot, depth + 1);
        }
    }
}
