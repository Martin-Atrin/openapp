//! What is on screen: presentations (§19.1), web surfaces (§13), layout / theme / history.

use crate::research::Kind;
use super::*;

/// module -> view names (loaded modules only) and all slot ids in spec views.
pub(crate) fn view_index(g: &Inner) -> (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>) {
    let mut mods = BTreeMap::new();
    let mut slots = BTreeSet::new();
    for m in g.modules.values() {
        if let Some(mf) = &m.manifest {
            mods.insert(m.id.clone(), mf.views.keys().cloned().collect());
        }
        for v in m.views.values() {
            if let Some(t) = v.tree() {
                views::collect_slots(t, &mut slots);
            }
        }
    }
    (mods, slots)
}

impl Kernel {
    /// (module, view) of a placement id, including presentations (`present:<id>`).
    pub fn placement_view(&self, placement_id: &str) -> Option<(String, String)> {
        // §21 G4: views in native windows
        if let Some(wid) = placement_id.strip_prefix("window:") {
            return crate::primitives::shell_windows::placement_view(self, wid);
        }
        let g = self.lock();
        if let Some(pid) = placement_id.strip_prefix("present:") {
            let p = g.presented.iter().find(|p| p["id"] == pid)?;
            let (m, v) = p["view"].as_str()?.split_once('/')?;
            return Some((m.to_string(), v.to_string()));
        }
        g.layout.placements.iter().find(|p| p.id == placement_id).map(|p| (p.module.clone(), p.view.clone()))
    }

    /// Host action `view.open {view: "<module>/<view>", as?, title?, width?, region?}`.
    pub fn view_open(&self, src: &[&Value]) -> Result<Value, String> {
        let get = |k: &str| src.iter().find_map(|v| v.get(k).cloned()).filter(|v| !v.is_null());
        let view = get("view").and_then(|v| v.as_str().map(String::from)).ok_or("view.open needs \"view\": \"<module>/<view>\"")?;
        let (module, vname) = view.split_once('/').ok_or("\"view\" must be \"<module>/<view>\"")?;
        let as_ = get("as").and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "sheet".into());
        if !["sheet", "modal", "inline", "tab"].contains(&as_.as_str()) {
            return Err(format!("\"as\" must be sheet, modal, inline or tab, got \"{as_}\""));
        }
        {
            let g = self.lock();
            let m = g.modules.get(module).ok_or_else(|| format!("unknown module \"{module}\""))?;
            if !m.views.contains_key(vname) {
                return Err(format!("module \"{module}\" has no view \"{vname}\" (views: {})", m.views.keys().cloned().collect::<Vec<_>>().join(", ")));
            }
        }
        let title = get("title").and_then(|v| v.as_str().map(String::from));
        if as_ == "tab" {
            let region = get("region").and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "tabs".into());
            if !layout::valid_ident(&region) {
                return Err(format!("bad region id \"{region}\""));
            }
            let anchor = format!("region:{region}");
            let (exists, placed) = {
                let g = self.lock();
                (
                    g.layout.regions.iter().any(|r| r.id == region),
                    g.layout.placements.iter().find(|p| p.anchor == anchor && p.module == module && p.view == vname).map(|p| p.id.clone()),
                )
            };
            if let Some(pid) = placed {
                return Ok(json!({ "ok": true, "as": "tab", "placement": pid, "region": region }));
            }
            let mut ops = Vec::new();
            if !exists {
                ops.push(json!({ "op": "region_add", "id": region, "edge": "right", "size": get("width").and_then(|w| w.as_f64()).unwrap_or(420.0), "mode": "tabs" }));
            }
            let mut place = json!({ "op": "place", "module": module, "view": vname, "anchor": anchor });
            if let Some(t) = &title {
                place["label"] = json!(t);
            }
            ops.push(place);
            let r = self.layout_ops(&ops).map_err(|e| e["errors"].to_string())?;
            return Ok(json!({ "ok": true, "as": "tab", "region": region, "placed": r["placed"] }));
        }
        let id = format!("pr_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let mut p = json!({ "id": id, "view": view, "as": as_ });
        if let Some(t) = title {
            p["title"] = json!(t);
        }
        if let Some(w) = get("width").and_then(|w| w.as_f64()) {
            p["width"] = json!(w);
        }
        {
            let mut g = self.lock();
            if as_ == "inline" {
                // anchored after the current end of the active thread
                if let Some(last) = g.active_chat().last().and_then(|i| i["id"].as_str().map(String::from)) {
                    p["after"] = json!(last);
                }
            }
            g.presented.push(p.clone());
        }
        self.research.log("user", Kind::Present, json!({ "op": "open", "view": view, "as": p["as"] }));
        self.emit_snapshot();
        Ok(json!({ "ok": true, "presented": p }))
    }

    /// Host action `view.close {view?}`: closes the top presentation (of that view, if given).
    pub fn view_close(&self, view: Option<&str>) -> Result<Value, String> {
        let closed = {
            let mut g = self.lock();
            let idx = g.presented.iter().rposition(|p| view.is_none_or(|v| p["view"] == v || p["id"] == v));
            idx.map(|i| g.presented.remove(i))
        };
        let c = closed.ok_or("nothing is open")?;
        {
            let mut g = self.lock();
            let pid = format!("present:{}", c["id"].as_str().unwrap_or(""));
            if g.guide.as_ref().is_some_and(|gd| gd["placement"] == pid.as_str()) {
                g.guide = None;
            }
        }
        self.research.log("user", Kind::Present, json!({ "op": "close", "view": c["view"] }));
        self.emit_snapshot();
        Ok(json!({ "ok": true, "closed": c }))
    }

    /// (url, profile) of a placement whose view is a web view.
    pub fn web_view_of(&self, placement: &str) -> Option<(String, String)> {
        let (module, view) = self.placement_view(placement)?;
        let g = self.lock();
        let v = g.modules.get(&module)?.views.get(&view)?;
        if v.value.get("kind").and_then(Value::as_str) != Some("web") {
            return None;
        }
        Some((v.value["url"].as_str()?.to_string(), v.value["profile"].as_str().unwrap_or(&module).to_string()))
    }

    /// Shell command `web_bounds`.
    pub fn web_bounds(&self, placement: &str, rect: Option<WebRect>) -> Result<(), String> {
        let host = self.web_host.get().ok_or("web surfaces are not available in this build")?;
        let (url, profile) = self
            .web_view_of(placement)
            .ok_or_else(|| format!("placement \"{placement}\" is not a web view"))?;
        let live = self.web_live.locked().contains(placement);
        if rect.is_none() && !live {
            return Ok(());
        }
        // a hidden-but-live surface keeps its current page; the start URL is used only on creation
        let start = {
            let g = self.lock();
            g.web.get(placement).and_then(|w| w["url"].as_str().map(String::from)).unwrap_or(url)
        };
        host.set_bounds(placement, &start, &profile, rect)?;
        if rect.is_some() && !live {
            self.web_live.locked().insert(placement.to_string());
            self.lock().web.entry(placement.to_string()).or_insert_with(|| json!({ "url": start, "title": "", "loading": true }));
        }
        Ok(())
    }

    /// Called by the web host on page-load / title events.
    pub fn web_update(&self, placement: &str, url: Option<&str>, title: Option<&str>, loading: Option<bool>) {
        {
            let mut g = self.lock();
            let e = g.web.entry(placement.to_string()).or_insert_with(|| json!({ "url": "", "title": "", "loading": false }));
            if let Some(u) = url {
                e["url"] = json!(u);
            }
            if let Some(t) = title {
                e["title"] = json!(t);
            }
            if let Some(l) = loading {
                e["loading"] = json!(l);
            }
        }
        self.emit_snapshot();
    }

    /// Destroy surfaces whose placement is gone or no longer a web view.
    pub(crate) fn sync_web(&self) {
        let Some(host) = self.web_host.get() else { return };
        let live: Vec<String> = self.web_live.locked().iter().cloned().collect();
        for p in live {
            if self.web_view_of(&p).is_none() {
                host.destroy(&p);
                self.web_live.locked().remove(&p);
                self.lock().web.remove(&p);
            }
        }
    }

    pub(crate) fn web_action(&self, action: &str, placement: &str, url: Option<String>) -> Result<(), String> {
        let host = self.web_host.get().ok_or("web surfaces are not available in this build")?;
        if self.web_view_of(placement).is_none() {
            return Err(format!("placement \"{placement}\" is not a web view"));
        }
        match action {
            "web.navigate" => {
                let url = url.ok_or("web.navigate needs \"url\"")?;
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err("web.navigate only opens http(s) URLs".into());
                }
                if !self.web_live.locked().contains(placement) {
                    // not mounted yet: remember the URL for creation
                    self.lock().web.insert(placement.to_string(), json!({ "url": url, "title": "", "loading": true }));
                    self.emit_snapshot();
                    return Ok(());
                }
                host.navigate(placement, &url)
            }
            "web.back" => host.history(placement, -1),
            "web.forward" => host.history(placement, 1),
            _ => host.history(placement, 0),
        }
    }

    pub(crate) fn placement_module(&self, placement_id: &str) -> Option<String> {
        self.placement_view(placement_id).map(|(m, _)| m)
    }

    pub(crate) fn push_history(&self, g: &mut Inner) {
        let e = HistEntry { ts: now_ts(), layout: g.layout.clone(), theme: g.theme.clone() };
        if let Some(tid) = self.build_turn() {
            let mut t = self.turns.locked();
            if let Some(r) = t.get_mut(&tid) {
                if r.before.is_none() {
                    r.before = Some(e.clone());
                }
                r.changes += 1;
            }
        }
        self.store.append_history(&e);
        g.history.push(e);
        if g.history.len() > crate::state::HISTORY_CAP + 50 {
            let n = g.history.len() - crate::state::HISTORY_CAP;
            g.history.drain(..n);
            self.store.rewrite_history(&g.history);
        }
    }

    pub fn layout_ops(&self, ops: &[Value]) -> Result<Value, Value> {
        if ops.is_empty() {
            return Err(json!({ "ok": false, "errors": ["ops is empty"] }));
        }
        let res = {
            let mut g = self.lock();
            let (mods, slots) = view_index(&g);
            match layout::apply_ops(&g.layout, ops, &LayoutCtx { modules: &mods, slots: &slots }) {
                Ok((next, created)) => {
                    self.push_history(&mut g);
                    g.layout = next;
                    self.store.save_layout(&g.layout);
                    Ok(json!({ "ok": true, "layout": g.layout, "placed": created }))
                }
                Err(errors) => Err(json!({
                    "ok": false,
                    "errors": errors,
                    "hint": "No op was applied (the call is atomic). Fix the listed ops and send the whole list again."
                })),
            }
        };
        match &res {
            Ok(v) => {
                self.research.log("agent", Kind::Layout, json!({ "ops": ops, "placed": v["placed"] }));
                self.emit_snapshot();
            }
            Err(e) => self.research.log("agent", Kind::Layout, json!({ "ops": ops, "rejected": e["errors"] })),
        }
        res
    }

    pub fn theme_set(&self, tokens: &Map<String, Value>) -> Result<Value, Value> {
        let res = {
            let mut g = self.lock();
            match theme::merge_and_validate(&g.theme, tokens) {
                Ok(next) => {
                    self.push_history(&mut g);
                    g.theme = next;
                    self.store.save_theme(&g.theme);
                    Ok(json!({ "ok": true, "theme": theme::resolve(&g.theme), "overrides": g.theme }))
                }
                Err(problems) => Err(json!({ "ok": false, "errors": problems, "hint": "Nothing was changed. Adjust the values and try again." })),
            }
        };
        match &res {
            Ok(_) => {
                self.research.log("agent", Kind::Theme, json!({ "tokens": tokens }));
                self.emit_snapshot();
            }
            Err(e) => self.research.log("agent", Kind::Theme, json!({ "tokens": tokens, "rejected": e["errors"] })),
        }
        res
    }

    /// Revert the last `steps` layout/theme changes. `actor`: who asked (`agent` for openapp_undo, `user` for /undo).
    pub fn undo(&self, steps: usize, actor: &str) -> Value {
        let res = {
            let mut g = self.lock();
            let n = steps.max(1).min(g.history.len());
            if n == 0 {
                return json!({ "ok": false, "undone": 0, "error": "nothing to undo" });
            }
            let idx = g.history.len() - n;
            let target = g.history[idx].clone();
            g.history.truncate(idx);
            self.store.rewrite_history(&g.history);
            g.layout = target.layout;
            g.theme = target.theme;
            self.store.save_layout(&g.layout);
            self.store.save_theme(&g.theme);
            json!({ "ok": true, "undone": n, "remaining": g.history.len(), "layout": g.layout, "theme": theme::resolve(&g.theme) })
        };
        self.research.log(actor, Kind::Undo, json!({ "steps": steps, "undone": res["undone"] }));
        self.emit_snapshot();
        res
    }

    /// /reset: remove all regions + placements, reset the theme, clear the active session's
    /// transcript (the next prompt starts a fresh agent session). Modules and sessions stay.
    pub fn reset(&self) {
        let active = self.active_session();
        {
            let mut g = self.lock();
            self.push_history(&mut g);
            g.layout = Layout::default();
            g.theme = Map::new();
            if let Some(s) = g.sessions.get_mut(&active) {
                s.chat = Some(Vec::new());
                s.rec.agent_ids.clear();
                s.dirty = true;
            }
            g.ui_errors.clear();
            self.store.save_layout(&g.layout);
            self.store.save_theme(&g.theme);
        }
        crate::harness::forget_session(self, &active);
        self.save_index();
        self.flush_sessions();
        self.research.log("user", Kind::Layout, json!({ "reset": true }));
        self.emit_snapshot();
    }
}
