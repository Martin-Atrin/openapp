//! Host actions from views, the shell and the agent; UI errors.

use crate::research::Kind;
use super::*;
use crate::{modules, primitives};

pub(crate) const UI_ERROR_CAP: usize = 50;

impl Kernel {
    pub fn push_ui_error(&self, placement: &str, module: Option<&str>, message: &str) {
        let e = json!({ "ts": now_ts(), "placement": placement, "module": module, "message": message });
        {
            let mut g = self.lock();
            g.ui_errors.push_back(e.clone());
            while g.ui_errors.len() > UI_ERROR_CAP {
                g.ui_errors.pop_front();
            }
        }
        self.research.log("kernel", Kind::UiError, e);
    }

    pub fn report_ui_error(&self, placement_id: &str, message: &str) {
        let module = self.placement_module(placement_id);
        let msg: String = message.chars().take(2000).collect();
        self.push_ui_error(placement_id, module.as_deref(), &msg);
    }

    /// Run a host action: a built-in (chrome, audio, connect), a kernel method in the background, or a registry
    /// action in the background (errors as toasts). `src` carries the action object and the input.
    pub fn host_action(self: &Arc<Self>, host: &str, src: &[&Value]) -> Result<(), String> {
        if let Some(b) = primitives::builtin(host) {
            return (b.run)(self, src);
        }
        if primitives::connect::is_method(host) {
            return primitives::connect::spawn_method(self, host, src);
        }
        if primitives::action(host).is_some() {
            let (k, name, params) = (self.clone(), host.to_string(), Value::Object(primitives::src_params(src)));
            tokio::spawn(async move {
                if let Some(Err(e)) = primitives::call_action(&k, &name, params, primitives::Caller::new("user")).await {
                    k.notify(&e);
                }
            });
            return Ok(());
        }
        Err(format!("unknown host action \"{host}\" ({})", primitives::host_actions().join(", ")))
    }

    pub async fn ui_action(self: &Arc<Self>, placement_id: &str, action: Value, input: Option<Value>) -> Result<(), String> {
        self.ui_action_value(placement_id, action, input).await.map(|_| ())
    }

    /// `ui_action` with a result value (host.call results, view.open, turn.undo); Null otherwise.
    pub async fn ui_action_value(self: &Arc<Self>, placement_id: &str, action: Value, input: Option<Value>) -> Result<Value, String> {
        let host = action.get("host").and_then(Value::as_str).unwrap_or("");
        if !host.is_empty() && !primitives::host_actions().contains(&host) && crate::primitives::action(host).is_some() {
            // §21 registry host action (after the built-ins), with its result
            let mut params = action.as_object().cloned().unwrap_or_default();
            if let Some(Value::Object(inp)) = &input {
                params.extend(inp.clone());
            }
            let caller = crate::primitives::Caller::for_placement(self, placement_id);
            self.research.log("user", Kind::UiAction, json!({ "placement": placement_id, "module": caller.module(), "action": { "host": host } }));
            let res = crate::primitives::call_action(self, host, Value::Object(params), caller.clone()).await.unwrap_or_else(|| Err(format!("unknown host action \"{host}\"")));
            if let Err(e) = &res {
                self.push_ui_error(placement_id, caller.module(), &format!("action failed: {e}"));
            }
            return res;
        }
        if matches!(host, "host.call" | "turn.undo" | "view.open" | "view.close") || (primitives::connect::is_method(host) && !host.starts_with("guide.")) {
            let module = self.placement_module(placement_id);
            self.research.log("user", Kind::UiAction, json!({ "placement": placement_id, "module": module, "action": { "host": host } }));
            let empty = Value::Null;
            let inp = input.as_ref().unwrap_or(&empty);
            let get = |k: &str| action.get(k).or_else(|| inp.get(k)).cloned().filter(|v| !v.is_null());
            let res = match host {
                "host.call" => {
                    let method = get("method").and_then(|v| v.as_str().map(String::from)).ok_or("host.call needs \"method\"")?;
                    let target = get("module").and_then(|v| v.as_str().map(String::from));
                    let params = get("params").unwrap_or_else(|| json!({}));
                    let caller = module.as_deref().map(|m| format!("module:{m}")).unwrap_or_else(|| "user".into());
                    self.host_call(target.as_deref(), &method, params, &caller).await.map_err(String::from)
                }
                "turn.undo" => self.turn_undo(get("turnId").and_then(|v| v.as_str().map(String::from)).as_deref()).await,
                "view.open" => self.view_open(&[&action, inp]),
                m if primitives::connect::is_method(m) => {
                    let mut params = action.as_object().cloned().unwrap_or_default();
                    params.remove("host");
                    if let Some(o) = inp.as_object() {
                        for (k, v) in o {
                            params.insert(k.clone(), v.clone());
                        }
                    }
                    let caller = module.as_deref().map(|m| format!("module:{m}")).unwrap_or_else(|| "user".into());
                    primitives::connect::call(self, m, &Value::Object(params), &caller).await
                }
                _ => self.view_close(get("view").and_then(|v| v.as_str().map(String::from)).as_deref()),
            };
            if let Err(e) = &res {
                self.push_ui_error(placement_id, module.as_deref(), &format!("action failed: {e}"));
            }
            return res;
        }
        let module = self.placement_module(placement_id);
        self.research.log(
            "user",
            Kind::UiAction,
            json!({ "placement": placement_id, "module": module, "action": action }),
        );
        let audio = action.get("host").and_then(Value::as_str) == Some("audio.record");
        let res: Result<(), String> = if audio {
            // consent gate: the shell starts recording only after this returns ok (§15.1)
            match &module {
                Some(m) => primitives::audio::consent(self, m).await,
                None => Err(format!("audio.record needs a module placement, not \"{placement_id}\"")),
            }
        } else if let Some(tool) = action.get("tool").and_then(Value::as_str) {
            match &module {
                None => Err(format!("unknown placement \"{placement_id}\"")),
                Some(m) => {
                    let mut args = action.get("args").and_then(Value::as_object).cloned().unwrap_or_default();
                    if let Some(Value::Object(inp)) = &input {
                        for (k, v) in inp {
                            args.insert(k.clone(), v.clone());
                        }
                    }
                    match self.call_module_tool(m, tool, args).await {
                        Ok(r) => {
                            self.after_module_call(m, &r, true);
                            if r.is_error == Some(true) {
                                Err(format!("tool {m}/{tool} returned an error: {}", modules::result_text(&r)))
                            } else {
                                Ok(())
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
            }
        } else if let Some(host) = action.get("host").and_then(Value::as_str) {
            let empty = Value::Null;
            self.host_action(host, &[&action, input.as_ref().unwrap_or(&empty)])
        } else {
            Err("action needs \"tool\" or \"host\"".into())
        };
        if let Err(e) = &res {
            self.push_ui_error(placement_id, module.as_deref(), &format!("action failed: {e}"));
        }
        res.map(|_| Value::Null)
    }
}
