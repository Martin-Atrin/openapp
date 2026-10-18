//! G2 PTY (contract §21): a terminal in the app VM, streamed over G1 channels.
//!
//! `pty.open { module, command?: [...], cols?, rows?, cwd?, env? }` → `{ id, topic, input }`. The process runs
//! on a real pseudo-terminal (microsandbox exec with `tty: true`; no guest helper): default `bash -l` in
//! /workspace, `TERM=xterm-256color`, network through the egress proxy (actor `module.<id>`). The initial
//! size is applied with `stty` before the program starts (no 80x24 flash); later sizes with `pty.resize`
//! run `stty -F <tty> cols … rows …` in the guest (TIOCSWINSZ → SIGWINCH to the foreground group; the SDK's
//! exec resize message is sent too but microsandbox 0.7.8's guest agent does not apply it).
//!
//! - output: topic `<module>/pty/<id>` — binary messages (terminal bytes; coalesced by concatenation under
//!   the rate cap), then one JSON `{ "type": "exit", "code": n }` when the program ends.
//! - input: topic `<module>/pty/<id>/in` — publish binary or a JSON string (keystrokes as text), or JSON
//!   `{ "resize": { "cols", "rows" } }`. Views publish there with `openapp.publish` (their own module's topic).
//! - `pty.write { id, data }`, `pty.resize { id, cols, rows }`, `pty.close { id }`, `pty.list { module? }`.
//!   PTYs of a module close when it unloads.

use crate::research::Kind;
use crate::util::MutexExt;
use super::channels::{ChannelData, PublishOpts};
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::JoinHandle;

const TTY_DIR: &str = "/run/openapp/pty";

/// Set the terminal size (TIOCSWINSZ on the slave; the kernel sends SIGWINCH to the foreground group).
async fn resize_tty(k: &Kernel, id: &str, ctl: &microsandbox::ExecControl, cols: u16, rows: u16) -> Result<(), String> {
    let _ = ctl.resize(rows, cols).await;
    let vm = k.vm().ok_or("the app VM is not running")?;
    let (code, _, err) = vm.sh(&format!("for i in $(seq 40); do t=$(cat {TTY_DIR}/{id} 2>/dev/null); [ -n \"$t\" ] && break; sleep 0.05; done; stty -F \"$t\" cols {cols} rows {rows}"), &[]).await?;
    if code != 0 {
        return Err(format!("pty.resize: {}", err.trim()));
    }
    Ok(())
}

#[derive(Default)]
pub struct PtyState {
    next: AtomicU64,
    ptys: Mutex<BTreeMap<String, PtyRun>>,
}

struct PtyRun {
    module: String,
    command: Vec<String>,
    control: microsandbox::ExecControl,
    sink: Arc<microsandbox::sandbox::exec::ExecSink>,
    size: (u16, u16),
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for PtyRun {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn pstate(k: &Kernel) -> Arc<PtyState> {
    k.prims.get::<PtyState>()
}

fn topic(module: &str, id: &str) -> String {
    format!("{module}/pty/{id}")
}

fn dim(p: &Value, key: &str, def: u16) -> Result<u16, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(def),
        Some(v) => v.as_u64().filter(|n| (2..=1000).contains(n)).map(|n| n as u16).ok_or_else(|| format!("\"{key}\" must be a number (2–1000)")),
    }
}

async fn act_open(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let module = arg(&p, "module").or_else(|| c.module()).ok_or("\"module\" is required (the module whose view shows the terminal)")?.to_string();
    if !crate::modules::valid_module_id(&module) {
        return Err(format!("\"{module}\" is not a module id"));
    }
    super::services::allowed_module(&c, &module)?;
    let cols = dim(&p, "cols", 80)?;
    let rows = dim(&p, "rows", 24)?;
    let command: Vec<String> = match p.get("command") {
        None | Some(Value::Null) => vec!["bash".into(), "-l".into()],
        Some(Value::String(s)) => vec!["bash".into(), "-lc".into(), s.clone()],
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(_) => return Err("\"command\" must be an array of strings or a shell line".into()),
    };
    let cwd = match arg(&p, "cwd") {
        None => crate::vm::GUEST_WS.to_string(),
        Some(c) if c.starts_with('/') => c.to_string(),
        Some(c) if crate::modules::safe_rel(c) => format!("{}/{c}", crate::vm::GUEST_WS),
        Some(c) => return Err(format!("\"cwd\" {c:?} must be absolute or relative to /workspace")),
    };
    let vm = k.ensure_vm().await?;
    let mut env = crate::vm::proxy_env(&format!("module.{module}"));
    env.extend([
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("OPENAPP_MODULE".to_string(), module.clone()),
    ]);
    if let Some(e) = p.get("env").and_then(Value::as_object) {
        for (a, b) in e {
            if let Some(s) = b.as_str() {
                env.push((a.clone(), s.to_string()));
            }
        }
    }
    let st = pstate(&k);
    let id = format!("p{}", st.next.fetch_add(1, Ordering::Relaxed) + 1);
    // the tty path is recorded for resizes (`stty -F`; the exec resize message does not reach the guest pty)
    let mut args = vec![
        "-c".to_string(),
        format!("mkdir -p {TTY_DIR}; tty > {TTY_DIR}/{id}; stty cols {cols} rows {rows} 2>/dev/null; exec \"$@\""),
        "sh".to_string(),
    ];
    args.extend(command.iter().cloned());
    let mut h = vm.exec_tty("sh", &args, &env, Some(&cwd)).await?;
    let control = h.control();
    let sink = Arc::new(h.take_stdin().ok_or("pty: no stdin")?);
    let out_topic = topic(&module, &id);
    let in_topic = format!("{out_topic}/in");
    // output pump
    let weak = Arc::downgrade(&k);
    let (t_out, id2) = (out_topic.clone(), id.clone());
    let out_task = tokio::spawn(async move {
        let mut code = -1;
        while let Some(ev) = h.recv().await {
            match ev {
                microsandbox::ExecEvent::Stdout(d) | microsandbox::ExecEvent::Stderr(d) => {
                    let Some(k) = weak.upgrade() else { return };
                    super::channels::publish_with(&k, &t_out, ChannelData::Binary(d.to_vec()), "kernel", PublishOpts { concat: true, seq: None });
                }
                microsandbox::ExecEvent::Exited { code: c } => {
                    code = c;
                    break;
                }
                microsandbox::ExecEvent::Failed(f) => {
                    if let Some(k) = weak.upgrade() {
                        super::channels::publish(&k, &t_out, ChannelData::Binary(format!("\r\n[pty failed: {f:?}]\r\n").into_bytes()));
                    }
                    break;
                }
                _ => {}
            }
        }
        if let Some(k) = weak.upgrade() {
            super::channels::publish(&k, &t_out, ChannelData::Json(json!({ "type": "exit", "code": code })));
            let st = pstate(&k);
            // dropping the entry aborts the input pump (and this task, which has no await left)
            let run = st.ptys.locked().remove(&id2);
            drop(run);
            k.emit_snapshot();
        }
    });
    // input pump
    let mut rx = super::channels::subscribe(&k, &in_topic, false);
    let (sink2, ctl2) = (sink.clone(), control.clone());
    let weak = Arc::downgrade(&k);
    let id3 = id.clone();
    let in_task = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            match &m.data {
                ChannelData::Binary(b) => {
                    if sink2.write(b).await.is_err() {
                        break;
                    }
                }
                ChannelData::Json(Value::String(s)) => {
                    if sink2.write(s.as_bytes()).await.is_err() {
                        break;
                    }
                }
                ChannelData::Json(v) => {
                    if let Some(r) = v.get("resize") {
                        let (c, r) = (r.get("cols").and_then(Value::as_u64), r.get("rows").and_then(Value::as_u64));
                        if let (Some(c), Some(r)) = (c, r) {
                            let (c, r) = (c.clamp(2, 1000) as u16, r.clamp(2, 1000) as u16);
                            if let Some(k) = weak.upgrade() {
                                let _ = resize_tty(&k, &id3, &ctl2, c, r).await;
                                if let Some(run) = pstate(&k).ptys.locked().get_mut(&id3) {
                                    run.size = (c, r);
                                }
                            }
                        }
                    } else if let Some(d) = v.get("data").and_then(Value::as_str)
                        && sink2.write(d.as_bytes()).await.is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    st.ptys.locked().insert(
        id.clone(),
        PtyRun { module: module.clone(), command: command.clone(), control, sink, size: (cols, rows), tasks: vec![in_task, out_task] },
    );
    k.research.log(&c.id, Kind::PtyOpen, json!({ "module": module, "id": id, "command": command, "cols": cols, "rows": rows }));
    Ok(json!({ "id": id, "topic": out_topic, "input": in_topic, "cols": cols, "rows": rows, "cwd": cwd }))
}

fn get_run<T>(k: &Kernel, c: &Caller, p: &Value, f: impl FnOnce(&mut PtyRun) -> T) -> Result<T, String> {
    let id = arg(p, "id").ok_or("\"id\" is required (from pty.open)")?;
    let st = pstate(k);
    let mut g = st.ptys.locked();
    let run = g.get_mut(id).ok_or_else(|| format!("no open pty \"{id}\" (it may have exited)"))?;
    super::services::allowed_module(c, &run.module)?;
    Ok(f(run))
}

async fn act_resize(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let cols = dim(&p, "cols", 80)?;
    let rows = dim(&p, "rows", 24)?;
    let ctl = get_run(&k, &c, &p, |r| {
        r.size = (cols, rows);
        r.control.clone()
    })?;
    let id = arg(&p, "id").unwrap_or_default().to_string();
    resize_tty(&k, &id, &ctl, cols, rows).await?;
    Ok(json!({ "ok": true, "cols": cols, "rows": rows }))
}

async fn act_write(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let data = arg(&p, "data").ok_or("\"data\" (text) is required")?.to_string();
    let sink = get_run(&k, &c, &p, |r| r.sink.clone())?;
    sink.write(data.as_bytes()).await.map_err(|e| format!("pty.write: {e}"))?;
    Ok(json!({ "ok": true, "bytes": data.len() }))
}

async fn act_close(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let ctl = get_run(&k, &c, &p, |r| r.control.clone())?;
    let _ = ctl.signal(1).await; // SIGHUP, like a closed terminal
    let _ = ctl.kill().await;
    Ok(json!({ "ok": true }))
}

async fn act_list(k: Arc<Kernel>, p: Value, c: Caller) -> Result<Value, String> {
    let module = arg(&p, "module").map(String::from).or_else(|| c.module().map(String::from));
    let st = pstate(&k);
    let g = st.ptys.locked();
    let list: Vec<Value> = g
        .iter()
        .filter(|(_, r)| module.as_deref().is_none_or(|m| m == r.module))
        .map(|(id, r)| json!({ "id": id, "module": r.module, "command": r.command, "cols": r.size.0, "rows": r.size.1, "topic": topic(&r.module, id) }))
        .collect();
    Ok(json!({ "ptys": list }))
}

/// Close every PTY of a module (module unload).
pub fn close_module(k: &Kernel, module: &str) {
    let st = pstate(k);
    let ctls: Vec<microsandbox::ExecControl> = st.ptys.locked().values().filter(|r| r.module == module).map(|r| r.control.clone()).collect();
    for c in ctls {
        k.spawn(async move {
            let _ = c.kill().await;
        });
    }
}

pub fn primitive() -> Primitive {
    let actions = vec![
        Action { name: "pty.open", args: "{ module, command?: [...] | \"shell line\", cols?, rows?, cwd?, env? } → { id, topic: \"<module>/pty/<id>\" (output, binary), input: \"…/in\" }", handler: handler(act_open) },
        Action { name: "pty.resize", args: "{ id, cols, rows }", handler: handler(act_resize) },
        Action { name: "pty.write", args: "{ id, data } — type text into the terminal", handler: handler(act_write) },
        Action { name: "pty.close", args: "{ id }", handler: handler(act_close) },
        Action { name: "pty.list", args: "{ module? } → { ptys }", handler: handler(act_list) },
    ];
    let endpoints = vec![
        Endpoint { method: "pty.open", args: "{ command?, cols?, rows?, cwd?, env? }", handler: handler(act_open) },
        Endpoint { method: "pty.resize", args: "{ id, cols, rows }", handler: handler(act_resize) },
        Endpoint { method: "pty.write", args: "{ id, data }", handler: handler(act_write) },
        Endpoint { method: "pty.close", args: "{ id }", handler: handler(act_close) },
    ];
    Primitive {
        name: "pty",
        purpose: "Terminals in the app VM (bash in /workspace by default) streamed over live channels — for xterm.js views, REPLs, TUIs",
        example: "const t = await openapp.host(\"pty.open\", { cols: 100, rows: 30 }); openapp.subscribe(t.topic, m => term.write(m.bytes ?? \"\")); term.onData(d => openapp.publish(t.input, d))",
        class: Class::Free,
        actions,
        endpoints,
        data: vec![DataSource {
            name: "ptys",
            doc: "list items [{ id, title: command, subtitle: module · cols×rows, icon: \"terminal\", action: {host:\"pty.close\", id} }]",
            get: data_fn(|k| {
                let st = pstate(k);
                let g = st.ptys.locked();
                Value::Array(
                    g.iter()
                        .map(|(id, r)| json!({ "id": id, "title": r.command.join(" "), "subtitle": format!("{} · {}×{}", r.module, r.size.0, r.size.1), "icon": "terminal", "module": r.module, "topic": topic(&r.module, id), "action": { "host": "pty.close", "id": id } }))
                        .collect(),
                )
            }),
        }],
        ..Default::default()
    }
}
