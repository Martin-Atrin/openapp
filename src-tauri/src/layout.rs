//! Layout model and atomic ops (contract §5).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default)]
    pub regions: Vec<Region>,
    #[serde(default)]
    pub placements: Vec<Placement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub edge: String,
    pub size: f64,
    pub mode: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub id: String,
    pub module: String,
    pub view: String,
    pub anchor: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub order: f64,
}

pub const FIXED_ANCHORS: &[&str] = &["composer.actions", "composer.above", "main"];
const EDGES: &[&str] = &["left", "right", "top", "bottom"];
const MODES: &[&str] = &["stack", "tabs"];
const OPS: &[&str] = &["region_add", "region_update", "region_remove", "place", "move", "unplace"];

/// What exists right now, for validating ops.
pub struct LayoutCtx<'a> {
    /// module id -> view names
    pub modules: &'a BTreeMap<String, BTreeSet<String>>,
    /// slot ids found in spec views
    pub slots: &'a BTreeSet<String>,
}

pub fn valid_ident(s: &str) -> bool {
    crate::util::valid_ident(s, 64)
}

pub fn new_placement_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("p_{}", &u[..8])
}

/// Check an anchor against the layout being built and the known slots.
pub fn check_anchor(anchor: &str, layout: &Layout, slots: &BTreeSet<String>) -> Result<(), String> {
    if FIXED_ANCHORS.contains(&anchor) {
        return Ok(());
    }
    if let Some(r) = anchor.strip_prefix("region:") {
        if layout.regions.iter().any(|x| x.id == r) {
            return Ok(());
        }
        let ids: Vec<&str> = layout.regions.iter().map(|x| x.id.as_str()).collect();
        return Err(format!(
            "anchor \"{anchor}\": no region \"{r}\" (regions: {}). Add it first with region_add (it can be in the same ops list).",
            if ids.is_empty() { "none".to_string() } else { ids.join(", ") }
        ));
    }
    if let Some(s) = anchor.strip_prefix("slot:") {
        if slots.contains(s) {
            return Ok(());
        }
        return Err(format!(
            "anchor \"{anchor}\": no slot \"{s}\" in any loaded spec view (slots: {}). Add {{\"type\":\"slot\",\"id\":\"{s}\"}} to a spec view and load its module first.",
            if slots.is_empty() { "none".to_string() } else { slots.iter().cloned().collect::<Vec<_>>().join(", ") }
        ));
    }
    Err(format!(
        "unknown anchor \"{anchor}\" (use composer.actions, composer.above, main, region:<id> or slot:<id>)"
    ))
}

fn get_str<'a>(op: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match op.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(v) => Err(format!("\"{key}\" must be a string, got {v}")),
    }
}

fn req_str<'a>(op: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    get_str(op, key)?.ok_or_else(|| format!("missing \"{key}\""))
}

fn get_num(op: &Map<String, Value>, key: &str) -> Result<Option<f64>, String> {
    match op.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("\"{key}\" must be a number, got {v}")),
    }
}

fn check_fields(op: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    for k in op.keys() {
        if k != "op" && !allowed.contains(&k.as_str()) {
            return Err(format!("unknown field \"{k}\" (allowed: {})", allowed.join(", ")));
        }
    }
    Ok(())
}

fn next_order(layout: &Layout, anchor: &str) -> f64 {
    layout
        .placements
        .iter()
        .filter(|p| p.anchor == anchor)
        .map(|p| p.order)
        .fold(None, |m: Option<f64>, o| Some(m.map_or(o, |m| m.max(o))))
        .map_or(0.0, |m| m + 1.0)
}

fn apply_one(layout: &mut Layout, op: &Value, ctx: &LayoutCtx, created: &mut Vec<Value>, idx: usize) -> Result<(), String> {
    let obj = op.as_object().ok_or("op must be an object")?;
    let name = req_str(obj, "op")?;
    match name {
        "region_add" => {
            check_fields(obj, &["id", "edge", "size", "mode", "title"])?;
            let id = req_str(obj, "id")?;
            if !valid_ident(id) {
                return Err(format!("region id \"{id}\" must match [A-Za-z0-9_-]{{1,64}}"));
            }
            if layout.regions.iter().any(|r| r.id == id) {
                return Err(format!("region \"{id}\" already exists (use region_update)"));
            }
            let edge = req_str(obj, "edge")?;
            if !EDGES.contains(&edge) {
                return Err(format!("edge \"{edge}\" must be one of {}", EDGES.join(", ")));
            }
            let size = get_num(obj, "size")?.ok_or("missing \"size\" (px)")?;
            check_size(size)?;
            let mode = get_str(obj, "mode")?.unwrap_or("stack");
            if !MODES.contains(&mode) {
                return Err(format!("mode \"{mode}\" must be stack or tabs"));
            }
            layout.regions.push(Region {
                id: id.into(),
                edge: edge.into(),
                size,
                mode: mode.into(),
                title: get_str(obj, "title")?.map(String::from),
            });
        }
        "region_update" => {
            check_fields(obj, &["id", "size", "mode", "title"])?;
            let id = req_str(obj, "id")?;
            let size = get_num(obj, "size")?;
            let mode = get_str(obj, "mode")?;
            let title = get_str(obj, "title")?;
            let r = layout
                .regions
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or_else(|| format!("no region \"{id}\""))?;
            if let Some(s) = size {
                check_size(s)?;
                r.size = s;
            }
            if let Some(m) = mode {
                if !MODES.contains(&m) {
                    return Err(format!("mode \"{m}\" must be stack or tabs"));
                }
                r.mode = m.into();
            }
            if obj.contains_key("title") {
                r.title = title.map(String::from);
            }
        }
        "region_remove" => {
            check_fields(obj, &["id"])?;
            let id = req_str(obj, "id")?;
            let before = layout.regions.len();
            layout.regions.retain(|r| r.id != id);
            if layout.regions.len() == before {
                return Err(format!("no region \"{id}\""));
            }
            let anchor = format!("region:{id}");
            layout.placements.retain(|p| p.anchor != anchor);
        }
        "place" => {
            check_fields(obj, &["module", "view", "anchor", "label", "order"])?;
            let module = req_str(obj, "module")?;
            let view = req_str(obj, "view")?;
            let anchor = req_str(obj, "anchor")?;
            let views = ctx.modules.get(module).ok_or_else(|| {
                format!(
                    "unknown module \"{module}\" (loaded modules: {}). Write modules/{module}/openapp.json and call openapp_module_load first.",
                    join_keys(ctx.modules.keys())
                )
            })?;
            if !views.contains(view) {
                return Err(format!(
                    "module \"{module}\" has no view \"{view}\" (views: {})",
                    join_keys(views.iter())
                ));
            }
            check_anchor(anchor, layout, ctx.slots)?;
            let order = match get_num(obj, "order")? {
                Some(o) => o,
                None => next_order(layout, anchor),
            };
            let id = new_placement_id();
            layout.placements.push(Placement {
                id: id.clone(),
                module: module.into(),
                view: view.into(),
                anchor: anchor.into(),
                label: get_str(obj, "label")?.map(String::from),
                order,
            });
            created.push(json!({ "op_index": idx, "placement": id, "module": module, "view": view, "anchor": anchor }));
        }
        "move" => {
            check_fields(obj, &["placement", "anchor", "order"])?;
            let pid = req_str(obj, "placement")?;
            let anchor = req_str(obj, "anchor")?;
            check_anchor(anchor, layout, ctx.slots)?;
            let order = get_num(obj, "order")?;
            let default_order = next_order(layout, anchor);
            let p = layout
                .placements
                .iter_mut()
                .find(|p| p.id == pid)
                .ok_or_else(|| format!("no placement \"{pid}\""))?;
            let same_anchor = p.anchor == anchor;
            p.anchor = anchor.into();
            p.order = order.unwrap_or(if same_anchor { p.order } else { default_order });
        }
        "unplace" => {
            check_fields(obj, &["placement"])?;
            let pid = req_str(obj, "placement")?;
            let before = layout.placements.len();
            layout.placements.retain(|p| p.id != pid);
            if layout.placements.len() == before {
                return Err(format!("no placement \"{pid}\""));
            }
        }
        other => return Err(format!("unknown op \"{other}\" (ops: {})", OPS.join(", "))),
    }
    Ok(())
}

fn check_size(size: f64) -> Result<(), String> {
    if !(24.0..=2400.0).contains(&size) {
        return Err(format!("size {size} px must be between 24 and 2400"));
    }
    Ok(())
}

fn join_keys<'a>(it: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = it.map(|s| s.as_str()).collect();
    if v.is_empty() { "none".into() } else { v.join(", ") }
}

/// Validate all ops against a copy of the layout. All-or-nothing.
pub fn apply_ops(layout: &Layout, ops: &[Value], ctx: &LayoutCtx) -> Result<(Layout, Vec<Value>), Vec<String>> {
    let mut next = layout.clone();
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let name = op.get("op").and_then(Value::as_str).unwrap_or("?");
        if let Err(e) = apply_one(&mut next, op, ctx, &mut created, i) {
            errors.push(format!("ops[{i}] ({name}): {e}"));
        }
    }
    if errors.is_empty() { Ok((next, created)) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_ops() {
        let mut modules = BTreeMap::new();
        modules.insert("hello".to_string(), BTreeSet::from(["button".to_string()]));
        let slots = BTreeSet::new();
        let ctx = LayoutCtx { modules: &modules, slots: &slots };
        let ops = vec![
            json!({"op":"region_add","id":"left","edge":"left","size":280,"mode":"tabs"}),
            json!({"op":"place","module":"hello","view":"button","anchor":"region:left"}),
        ];
        let (l, created) = apply_ops(&Layout::default(), &ops, &ctx).unwrap();
        assert_eq!(l.regions.len(), 1);
        assert_eq!(created.len(), 1);
        let bad = vec![ops[0].clone(), json!({"op":"place","module":"nope","view":"x","anchor":"main"})];
        assert!(apply_ops(&Layout::default(), &bad, &ctx).is_err());
    }
}
