//! Module lifecycle: load (manifest, server, host part, views, default placements), unload, tool calls, view refresh, the watcher tick.

use crate::research::Kind;
use super::*;
use crate::modules;

/// §18.1 host-module consent cards in flight (`<module>:<source hash>`), per kernel.
#[derive(Default)]
struct HostPending(Mutex<BTreeSet<String>>);

fn host_pending(k: &Kernel) -> Arc<HostPending> {
    k.prims.get::<HostPending>()
}

impl Kernel {
    /// §18.1: approve (card) and build the host part of a module. Runs before the load lock is taken.
    pub(crate) async fn prepare_host(self: &Arc<Self>, id: &str, spec: &modules::HostSpec) -> Result<PathBuf, Value> {
        use crate::host_modules as hm;
        let dir = self.module_dir(id);
        let src = hm::collect(&dir, spec).map_err(|e| json!({ "error": e }))?;
        match hm::approval(id, &src.hash, &self.workspace) {
            hm::Approval::Allowed => {}
            hm::Approval::Denied => {
                return Err(json!({ "error": "the user denied this host code (this exact source). Change the code only if the user asks, then load again to ask once more.", "source_hash": src.hash }));
            }
            hm::Approval::Unknown => {
                let key = format!("{id}:{}", src.hash);
                if !host_pending(self).0.locked().insert(key.clone()) {
                    return Err(json!({ "error": "waiting for the user to allow this host code (the card is open); call openapp_module_load again after they answer" }));
                }
                let (imports, calls) = hm::scan(&src.files);
                let prev = hm::last_approved(id);
                let diff = hm::diff(&prev, &src.files);
                let name = modules::peek_name(&dir, id);
                let summary = spec.summary.clone().unwrap_or_default();
                let mut text = format!("{summary}\nFrameworks: {}", if imports.is_empty() { "none".into() } else { imports.join(", ") });
                if !calls.is_empty() {
                    text.push_str(&format!("\nAlso: {}", calls.join("; ")));
                }
                // §21 G8: declared native windows / a long-running service
                let raw_host = modules::read_manifest(&dir, id).ok().map(|m| m.raw["host"].clone()).unwrap_or(Value::Null);
                if raw_host["windows"] == true && !calls.iter().any(|c| c.contains("windows")) {
                    text.push_str("\nOpens its own windows.");
                }
                if raw_host["service"] == true {
                    text.push_str("\nKeeps running in the background while OpenApp is open.");
                }
                text.push_str(if prev.is_empty() { "\nNew code. It runs on your Mac with your permissions." } else { "\nThe code changed since you allowed it." });
                let req = crate::consent::ConsentReq::new("host_module", format!("{name} wants to run code on your Mac"), text)
                    .card("diff", json!(diff))
                    .card("details", json!({ "module": id, "summary": summary, "imports": imports, "calls": calls, "files": src.files.keys().collect::<Vec<_>>(), "source_hash": src.hash }))
                    .info(json!({ "module": id, "hash": src.hash, "imports": imports, "calls": calls }));
                let (done_tx, done_rx) = oneshot::channel::<bool>();
                let k = self.clone();
                let (mid, hash) = (id.to_string(), src.hash.clone());
                let files = src.files.clone();
                let package = src.package.clone();
                tokio::spawn(async move {
                    let d = crate::consent::ask(&k, req).await;
                    if d.choice.is_some() {
                        let s = hm::Sources { files, hash: hash.clone(), package };
                        let _ = hm::record(&mid, &s, &k.workspace, d.allowed);
                        k.grants.set("host_modules", &mid, d.allowed, json!({ "source_hash": hash }));
                        k.emit_snapshot();
                    }
                    host_pending(&k).0.locked().remove(&format!("{mid}:{hash}"));
                    let _ = done_tx.send(d.allowed);
                });
                match tokio::time::timeout(std::time::Duration::from_secs(150), done_rx).await {
                    Ok(Ok(true)) => {}
                    Ok(_) => return Err(json!({ "error": "the user denied this host code" })),
                    Err(_) => return Err(json!({ "error": "the user has not answered the consent card yet; call openapp_module_load again after they allow it" })),
                }
            }
        }
        let t0 = std::time::Instant::now();
        let bin = hm::compile(id, &src, spec).await?;
        self.research.log(&format!("module:{id}"), Kind::HostModuleBuild, json!({ "hash": src.hash, "ms": t0.elapsed().as_millis() as u64 }));
        Ok(bin)
    }

    pub async fn iframe_call(self: &Arc<Self>, module: &str, tool: &str, args: Value) -> Result<Value, String> {
        let args = match args {
            Value::Object(o) => o,
            Value::Null => Map::new(),
            _ => return Err("args must be an object".into()),
        };
        let r = self.call_module_tool(module, tool, args).await.inspect_err(|e| {
            self.push_ui_error("iframe", Some(module), &format!("openapp.call(\"{tool}\") failed: {e}"));
        })?;
        self.after_module_call(module, &r, true);
        serde_json::to_value(&r).map_err(|e| e.to_string())
    }

    /// Apply `structuredContent.openapp` directives and re-read the module's views.
    pub fn after_module_call(self: &Arc<Self>, module: &str, r: &CallToolResult, allow_prompt: bool) -> Vec<String> {
        let mut applied = Vec::new();
        if let Some(d) = r.structured_content.as_ref().and_then(|s| s.get("openapp")) {
            if let Some(t) = d.get("composer_insert").and_then(Value::as_str) {
                self.sink.emit("oa://composer", json!({ "insert": t }));
                applied.push("composer_insert".into());
            }
            if let Some(t) = d.get("notify").and_then(Value::as_str) {
                self.notify(t);
                applied.push("notify".into());
            }
            if let Some(t) = d.get("prompt").and_then(Value::as_str) {
                if allow_prompt {
                    self.send_prompt(t.to_string());
                    applied.push("prompt".into());
                } else {
                    applied.push("prompt (skipped: agent test call)".into());
                }
            }
        }
        if self.refresh_views(module, false) {
            self.emit_snapshot();
        }
        applied
    }

    pub async fn call_module_tool(self: &Arc<Self>, module: &str, tool: &str, args: Map<String, Value>) -> Result<CallToolResult, String> {
        // §21 G9: lazy activation on the first action
        crate::primitives::lifecycle::ensure_active(self, module).await?;
        let client = {
            let g = self.lock();
            let m = g.modules.get(module).ok_or_else(|| {
                format!("unknown module \"{module}\" (modules: {})", g.modules.keys().cloned().collect::<Vec<_>>().join(", "))
            })?;
            if m.manifest.as_ref().is_some_and(|mf| mf.server.is_none()) {
                return Err(format!("module \"{module}\" has no server (add \"server\" to openapp.json and call openapp_module_load)"));
            }
            let c = match (&m.client, m.status.as_str()) {
                (Some(c), "running") => c.clone(),
                _ => {
                    return Err(format!(
                        "module \"{module}\" is {}{}; call openapp_module_load",
                        m.status,
                        m.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
                    ));
                }
            };
            if !m.tools.iter().any(|t| t.name == tool) {
                return Err(format!(
                    "module \"{module}\" has no tool \"{tool}\" (tools: {})",
                    m.tool_names().into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            c
        };
        let r = modules::call_tool(&client, tool, args).await;
        if let Err(e) = &r {
            self.research.log(&format!("module:{module}"), Kind::ModuleError, json!({ "tool": tool, "error": e }));
        }
        r
    }

    pub(crate) fn module_dir(&self, id: &str) -> PathBuf {
        self.workspace.join("modules").join(id)
    }

    pub(crate) fn log_path(&self, id: &str) -> PathBuf {
        self.workspace.join(".openapp").join("logs").join(format!("{id}.log"))
    }

    pub async fn module_load(self: &Arc<Self>, id: &str) -> Result<Value, Value> {
        self.module_load_opts(id, true).await
    }

    /// `start = false` (§21 G9 lazy activation): read the manifest and views, apply default placements, but do not
    /// start the server or the host part; status becomes "idle" until the first view/action activates it.
    pub async fn module_load_opts(self: &Arc<Self>, id: &str, start: bool) -> Result<Value, Value> {
        if crate::primitives::lifecycle::safe_mode(self) {
            return Err(json!({ "ok": false, "id": id, "error": "safe mode is on: modules do not start. The user types /safe off to leave it." }));
        }
        // §18.1 host part: consent + Swift build happen before the load lock (the card may wait for the user)
        let mut host_bin: Option<Result<PathBuf, Value>> = None;
        if start && modules::valid_module_id(id)
            && let Ok(mf) = modules::read_manifest(&self.module_dir(id), id)
                && let Some(h) = &mf.host {
                    host_bin = Some(self.prepare_host(id, h).await);
                }
        if let Some(Err(e)) = &host_bin {
            let mut e = e.clone();
            e["ok"] = json!(false);
            e["id"] = json!(id);
            e["phase"] = json!("host");
            {
                let mut g = self.lock();
                if let Some(m) = g.modules.get_mut(id) {
                    m.status = "error".into();
                    m.error = e["error"].as_str().map(String::from);
                }
            }
            self.research.log(&format!("module:{id}"), Kind::ModuleError, json!({ "phase": "host", "error": e["error"], "diagnostics": e["diagnostics"] }));
            self.emit_snapshot();
            return Err(e);
        }
        let _guard = self.load_lock.lock().await;
        if !modules::valid_module_id(id) {
            return Err(json!({ "ok": false, "id": id, "error": format!("\"{id}\" is not a valid module id; use [a-z0-9-]+ and the same folder name") }));
        }
        let dir = self.module_dir(id);
        if !dir.is_dir() {
            let known = workspace::module_dirs(&self.workspace);
            return Err(json!({ "ok": false, "id": id, "error": format!("folder modules/{id}/ does not exist (module folders: {})", if known.is_empty() { "none".into() } else { known.join(", ") }) }));
        }
        // stop the old server
        let (old, old_host) = {
            let mut g = self.lock();
            let name = modules::peek_name(&dir, id);
            let e = g.modules.entry(id.to_string()).or_insert_with(|| ModuleEntry::new(id, name));
            (e.client.take(), e.host.take())
        };
        if let Some(h) = old_host {
            h.kill();
        }
        if let Some(c) = old {
            c.cancellation_token().cancel();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let manifest = match modules::read_manifest(&dir, id) {
            Ok(m) => m,
            Err(problems) => {
                {
                    let mut g = self.lock();
                    if let Some(e) = g.modules.get_mut(id) {
                        e.status = "error".into();
                        e.error = Some(problems.join("; "));
                    }
                }
                self.emit_snapshot();
                self.research.log(&format!("module:{id}"), Kind::ModuleError, json!({ "phase": "manifest", "problems": problems }));
                return Err(json!({ "ok": false, "id": id, "error": "invalid manifest", "problems": problems }));
            }
        };
        let tail: Tail = Arc::new(Mutex::new(VecDeque::new()));
        let mut tools = Vec::new();
        let mut client = None;
        let mut server_error = None;
        // §17: module servers run in the app VM when it runs (always for a VM harness)
        let vm = self.vm();
        let in_vm = vm.is_some();
        if let Some(server) = manifest.server.as_ref().filter(|_| start) {
            let henv = self.host_env(id, vm.is_some());
            // §21 G7: released `file:` secrets are in the VM before the server starts
            if let Some(vm) = &vm {
                crate::primitives::secret_release::deliver_files(self, vm, id).await;
            }
            let started = match (&vm, self.vm_required()) {
                (Some(vm), _) => modules::start_server_vm(vm, &dir, id, server, self.log_path(id), tail.clone(), &henv).await,
                (None, true) => Err(format!(
                    "the app VM is not running{}; module servers of this provider run only in the VM",
                    self.vm_error.locked().as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
                )),
                (None, false) => modules::start_server(&dir, id, server, self.path_env(), self.log_path(id), tail.clone(), &henv).await,
            };
            match started {
                Ok((c, t)) => {
                    client = Some(c);
                    tools = t;
                }
                Err(e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    server_error = Some(e);
                }
            }
        }
        // §18.1 host part: supervised sidecar on the Mac
        let mut host_sc = None;
        let mut host_methods = Value::Null;
        if let Some(Ok(bin)) = &host_bin {
            let sc = crate::host_services::Sidecar::new(
                &format!("host module {id}"),
                bin.clone(),
                self.workspace.join(".openapp").join("logs").join(format!("{id}.host.log")),
                vec![("OPENAPP_MODULE".into(), id.into()), ("OPENAPP_WORKSPACE".into(), self.workspace.to_string_lossy().to_string())],
                Some(dir.clone()),
            );
            sc.set_own_responsibility();
            // the sources just approved and compiled (same hash): their privacy-prompt frameworks
            if let Some(src) = manifest.host.as_ref().and_then(|h| crate::host_modules::collect(&dir, h).ok()) {
                sc.set_tcc_frameworks(crate::host_modules::tcc_frameworks(&src.files));
            }
            match sc.call("methods", json!({}), std::time::Duration::from_secs(15)).await {
                Ok(v) => host_methods = v,
                Err(e) if e.contains("unknown method") || e.contains("-32601") => host_methods = json!("(no methods listing)"),
                Err(e) if !sc.is_running() => {
                    server_error.get_or_insert(format!("host part did not start: {e}"));
                }
                Err(e) => host_methods = json!(format!("(methods failed: {e})")),
            }
            host_sc = Some(sc);
        }
        let tool_names: BTreeSet<String> = tools.iter().map(|t| t.name.to_string()).collect();
        let check_tools = start && manifest.server.is_some() && server_error.is_none();
        let mut new_views = BTreeMap::new();
        for (vn, spec) in &manifest.views {
            let rev = self.next_seq();
            new_views.insert(vn.clone(), modules::read_view(&dir, id, spec, check_tools.then_some(&tool_names), rev));
        }
        let files = modules::scan_files(&dir);
        let mut warnings: Vec<String> = Vec::new();
        let mut views_out = Map::new();
        for (vn, v) in &new_views {
            let mut o = json!({ "kind": v.kind, "file": v.file });
            if v.value.get("kind").and_then(Value::as_str) == Some("error") {
                o["error"] = v.value["error"].clone();
                warnings.push(format!("view {vn}: {}", v.value["error"].as_str().unwrap_or("")));
            }
            if !v.problems.is_empty() {
                o["problems"] = json!(v.problems);
                warnings.extend(v.problems.iter().cloned());
            }
            views_out.insert(vn.clone(), o);
        }
        let mut placed = Vec::new();
        let result;
        {
            let mut g = self.lock();
            let e = g.modules.entry(id.to_string()).or_insert_with(|| ModuleEntry::new(id, manifest.name.clone()));
            e.name = manifest.name.clone();
            e.tools = tools.clone();
            e.client = client;
            e.tail = tail.clone();
            e.views = new_views;
            e.files = files;
            e.in_vm = in_vm && manifest.server.is_some();
            e.host = host_sc.clone();
            e.host_methods = host_methods.clone();
            e.manifest = Some(manifest.clone());
            if let Some(err) = &server_error {
                e.status = "error".into();
                e.error = Some(err.clone());
            } else {
                e.status = if start || manifest.server.is_none() { "running" } else { "idle" }.into();
                e.error = None;
            }
            // default placements on first successful load
            if server_error.is_none() && !g.meta.defaults_applied.contains(id) {
                // §19.2: a module first loaded in a build turn is unloaded by that turn's Undo
                if let Some(tid) = self.build_turn()
                    && let Some(r) = self.turns.locked().get_mut(&tid)
                        && !r.modules.contains(&id.to_string()) {
                            r.modules.push(id.to_string());
                        }
                if !manifest.placements.is_empty() {
                    let before = g.layout.clone();
                    let (mods, slots) = view_index(&g);
                    let mut lay = g.layout.clone();
                    for p in &manifest.placements {
                        let mut op = json!({ "op": "place", "module": id, "view": p.view, "anchor": p.anchor });
                        if let Some(l) = &p.label {
                            op["label"] = json!(l);
                        }
                        if let Some(o) = p.order {
                            op["order"] = json!(o);
                        }
                        match layout::apply_ops(&lay, &[op], &LayoutCtx { modules: &mods, slots: &slots }) {
                            Ok((l, created)) => {
                                lay = l;
                                placed.extend(created.into_iter().filter_map(|c| c.get("placement").cloned()));
                            }
                            Err(errs) => warnings.extend(errs.into_iter().map(|x| format!("default placement skipped: {x}"))),
                        }
                    }
                    if lay != before {
                        self.push_history(&mut g);
                        g.layout = lay;
                        self.store.save_layout(&g.layout);
                    }
                }
                g.meta.defaults_applied.insert(id.to_string());
                self.store.save_meta(&g.meta);
            }
            let tools_json: Vec<Value> = tools.iter().map(|t| json!({ "name": t.name, "description": t.description })).collect();
            result = match &server_error {
                Some(err) => Err(json!({
                    "ok": false,
                    "id": id,
                    "status": "error",
                    "error": err,
                    "stderr": modules::tail_lines(&tail, 40),
                    "log": format!(".openapp/logs/{id}.log"),
                    "views": views_out,
                })),
                None => Ok(json!({
                    "ok": true,
                    "id": id,
                    "name": manifest.name,
                    "status": if start || manifest.server.is_none() { "running" } else { "idle" },
                    "tools": tools_json,
                    "views": views_out,
                    "placements_applied": placed,
                    "warnings": warnings,
                    "runs_in": if in_vm { "app VM (/workspace/modules/<id>)" } else { "host" },
                    "host_part": if host_sc.is_some() { json!({ "running": true, "methods": host_methods, "call": format!("POST $OPENAPP_HOST_URL/{id}/<method> or MCP host_call"), "log": format!(".openapp/logs/{id}.host.log") }) } else { Value::Null },
                    "log": format!(".openapp/logs/{id}.log"),
                })),
            };
        }
        if result.is_ok() {
            crate::primitives::event(self, &crate::primitives::Event::ModuleLoaded(id));
        }
        self.emit_snapshot();
        match &result {
            Ok(_) => self.research.log(
                &format!("module:{id}"),
                Kind::ModuleLoad,
                json!({ "tools": tool_names, "views": manifest.views.keys().collect::<Vec<_>>(), "placements": placed, "warnings": warnings.len() }),
            ),
            Err(e) => self.research.log(&format!("module:{id}"), Kind::ModuleError, json!({ "phase": "start", "error": e["error"], "stderr": e["stderr"] })),
        }
        result
    }

    pub async fn module_unload(self: &Arc<Self>, id: &str, remove_placements: bool) -> Result<Value, Value> {
        let _guard = self.load_lock.lock().await;
        let (client, removed) = {
            let mut g = self.lock();
            let Some(e) = g.modules.get_mut(id) else {
                return Err(json!({ "ok": false, "error": format!("unknown module \"{id}\"") }));
            };
            let c = e.client.take();
            if let Some(h) = e.host.take() {
                h.kill();
            }
            e.status = "stopped".into();
            e.error = None;
            let mut removed = 0;
            if remove_placements {
                let n = g.layout.placements.iter().filter(|p| p.module == id).count();
                if n > 0 {
                    self.push_history(&mut g);
                    g.layout.placements.retain(|p| p.module != id);
                    self.store.save_layout(&g.layout);
                    removed = n;
                }
            }
            (c, removed)
        };
        if let Some(c) = client {
            c.cancellation_token().cancel();
        }
        crate::primitives::event(self, &crate::primitives::Event::ModuleUnloaded(id));
        self.emit_snapshot();
        self.research.log(&format!("module:{id}"), Kind::ModuleLoad, json!({ "unloaded": true, "removed_placements": removed }));
        Ok(json!({ "ok": true, "id": id, "status": "stopped", "removed_placements": removed }))
    }

    /// Re-read changed views of one module. `force` re-reads spec views even if mtime is unchanged.
    pub fn refresh_views(&self, id: &str, force: bool) -> bool {
        let (dir, specs, tools, cur) = {
            let g = self.lock();
            let Some(m) = g.modules.get(id) else { return false };
            let Some(mf) = &m.manifest else { return false };
            let tools = (mf.server.is_some() && m.status == "running").then(|| m.tool_names());
            let cur: BTreeMap<String, (Option<SystemTime>, u64)> = m.views.iter().map(|(k, v)| (k.clone(), (v.mtime, v.rev))).collect();
            (self.module_dir(id), mf.views.clone(), tools, cur)
        };
        let mut updates = Vec::new();
        for (vn, spec) in &specs {
            if spec.kind == "web" {
                continue;
            }
            let mt = modules::mtime(&dir.join(&spec.file));
            let (old_mt, old_rev) = cur.get(vn).cloned().unwrap_or((None, 0));
            if mt == old_mt && !(force && spec.kind == "spec") {
                continue;
            }
            let rev = if mt != old_mt { self.next_seq() } else { old_rev };
            updates.push((vn.clone(), modules::read_view(&dir, id, spec, tools.as_ref(), rev)));
        }
        if updates.is_empty() {
            return false;
        }
        let mut g = self.lock();
        let Some(m) = g.modules.get_mut(id) else { return false };
        let mut changed = false;
        for (vn, v) in updates {
            if m.views.get(&vn).map(|o| o.value != v.value || o.problems != v.problems).unwrap_or(true) {
                changed = true;
            }
            m.views.insert(vn, v);
        }
        changed
    }

    /// One watcher tick. Returns true when the snapshot changed.
    pub fn poll_changes(&self) -> bool {
        self.flush_sessions();
        let mut changed = false;
        let on_disk = workspace::module_dirs(&self.workspace);
        // discover new / removed module folders
        {
            let mut g = self.lock();
            for id in &on_disk {
                if !g.modules.contains_key(id) {
                    let name = modules::peek_name(&self.module_dir(id), id);
                    g.modules.insert(id.clone(), ModuleEntry::new(id, name));
                    changed = true;
                }
            }
            let gone: Vec<String> = g.modules.keys().filter(|k| !on_disk.contains(k)).cloned().collect();
            for id in gone {
                if let Some(e) = g.modules.remove(&id)
                    && let Some(c) = e.client {
                        c.cancellation_token().cancel();
                    }
                changed = true;
            }
            // crashed servers
            for m in g.modules.values_mut() {
                if m.status == "running" && m.client.as_ref().is_some_and(|c| c.is_closed()) {
                    let tail = modules::tail_lines(&m.tail, 10).join("\n");
                    m.status = "error".into();
                    m.error = Some(format!("server exited. Last stderr:\n{tail}"));
                    m.client = None;
                    self.research.log(&format!("module:{}", m.id), Kind::ModuleError, json!({ "phase": "exit", "stderr": tail }));
                    changed = true;
                }
            }
        }
        // file changes of loaded modules
        let loaded: Vec<(String, BTreeMap<PathBuf, SystemTime>, BTreeSet<PathBuf>)> = {
            let g = self.lock();
            g.modules
                .values()
                .filter_map(|m| {
                    let mf = m.manifest.as_ref()?;
                    let dir = self.module_dir(&m.id);
                    let mut skip: BTreeSet<PathBuf> = mf.views.values().filter(|v| v.kind == "spec").map(|v| dir.join(&v.file)).collect();
                    skip.insert(dir.join("openapp.json"));
                    Some((m.id.clone(), m.files.clone(), skip))
                })
                .collect()
        };
        for (id, old_files, skip) in loaded {
            let dir = self.module_dir(&id);
            let new_files = modules::scan_files(&dir);
            if new_files == old_files {
                continue;
            }
            // non-spec files changed (html/css/js/...): reload html views
            let other_changed = new_files
                .iter()
                .filter(|(p, _)| !skip.contains(*p))
                .any(|(p, t)| old_files.get(p) != Some(t))
                || old_files.keys().any(|p| !skip.contains(p) && !new_files.contains_key(p));
            if self.refresh_views(&id, false) {
                changed = true;
            }
            let mut g = self.lock();
            if let Some(m) = g.modules.get_mut(&id) {
                m.files = new_files;
                if other_changed {
                    for v in m.views.values_mut() {
                        if v.kind == "html" && v.value.get("kind").and_then(Value::as_str) == Some("html") {
                            v.rev = self.seq.fetch_add(1, Ordering::Relaxed);
                            v.value["rev"] = json!(v.rev);
                            changed = true;
                        }
                    }
                }
            }
        }
        changed
    }
}
