//! G1 live channels (contract §21): topics `<owner>/<name>` (owner = module id, `host:<module>` or `kernel`),
//! JSON or binary messages, a ring buffer of the last 64 messages per topic for late subscribers, drop-oldest
//! queues per subscriber, and a rate cap of 120 messages/s per topic (excess coalesced, last-wins).
//!
//! Paths: Rust [`publish`] / [`subscribe`]; module servers `POST $OPENAPP_HOST_URL/channel.publish` or the
//! WebSocket `$OPENAPP_CHANNEL_URL` (`/channel?token=`); host modules JSON-RPC notifications `channel.publish` /
//! `channel.subscribe` / `channel.unsubscribe` (kernel → host module: `channel.message`); views Tauri commands
//! `channel_subscribe` / `channel_unsubscribe` / `channel_publish` + event `oa://channel`.

use crate::util::MutexExt;
use super::*;
use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
use base64::Engine;
use std::collections::{HashSet, VecDeque};
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const RING: usize = 64;
pub const RATE_PER_SEC: usize = 120;
/// Per-subscriber queue; the oldest message is dropped when it is full.
pub const SUB_QUEUE: usize = 256;
const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const KERNEL_TOPICS: &[&str] = &["kernel/sessions", "kernel/agent", "kernel/jobs", "kernel/agents", "kernel/notify", "kernel/vm-image"];

#[derive(Clone, Debug, PartialEq)]
pub enum ChannelData {
    Json(Value),
    Binary(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PublishOpts {
    /// When coalescing, append binary payloads to the pending one instead of replacing it (byte streams, PTY).
    pub concat: bool,
    /// Publisher sequence number (kept as `seq`; otherwise the topic counter is used).
    pub seq: Option<u64>,
}

#[derive(Debug)]
pub struct Message {
    pub topic: String,
    pub seq: u64,
    /// ms since the Unix epoch
    pub ts: u64,
    pub from: String,
    pub data: ChannelData,
}

impl Message {
    /// `{ topic, seq, ts, data }` or `{ topic, seq, ts, b64 }`.
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "topic": self.topic, "seq": self.seq, "ts": self.ts });
        match &self.data {
            ChannelData::Json(d) => v["data"] = d.clone(),
            ChannelData::Binary(b) => v["b64"] = json!(base64::engine::general_purpose::STANDARD.encode(b)),
        }
        v
    }
    pub fn json(&self) -> Option<&Value> {
        match &self.data {
            ChannelData::Json(v) => Some(v),
            ChannelData::Binary(_) => None,
        }
    }
}

#[derive(Default)]
struct SubQ {
    q: Mutex<VecDeque<Arc<Message>>>,
    notify: tokio::sync::Notify,
    closed: AtomicBool,
    dropped: AtomicU64,
}

impl SubQ {
    fn push(&self, m: Arc<Message>) {
        {
            let mut q = self.q.locked();
            if q.len() >= SUB_QUEUE {
                q.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            q.push_back(m);
        }
        self.notify.notify_one();
    }
}

#[derive(Default)]
struct Topic {
    ring: VecDeque<Arc<Message>>,
    subs: Vec<(u64, String, Arc<SubQ>)>,
    /// delivery times inside the last second (sliding window of the rate cap)
    sent: VecDeque<Instant>,
    pending: Option<Message>,
    pending_concat: bool,
    flush_scheduled: bool,
    seq: u64,
    published: u64,
    delivered: u64,
    coalesced: u64,
}

/// The channel hub of one kernel (`k.prims.get::<Hub>()`).
#[derive(Default)]
pub struct Hub {
    topics: Mutex<HashMap<String, Topic>>,
    next_id: AtomicU64,
    /// shell (view) subscriptions: sub id → (topic, forwarder)
    shell: Mutex<HashMap<u64, (String, tokio::task::AbortHandle)>>,
    /// host-module subscriptions: module → topic → forwarder
    host: Mutex<HashMap<String, HashMap<String, tokio::task::AbortHandle>>>,
    /// open WebSocket subscriptions per module (lifecycle: "in use")
    ws: Mutex<HashMap<String, usize>>,
    last_kernel: Mutex<HashMap<&'static str, String>>,
}

pub fn hub(k: &Kernel) -> Arc<Hub> {
    k.prims.get::<Hub>()
}

/// Receives messages of one topic; dropping it unsubscribes.
pub struct Receiver {
    pub topic: String,
    id: u64,
    q: Arc<SubQ>,
    hub: Weak<Hub>,
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Arc<Message>> {
        loop {
            if let Some(m) = self.q.q.locked().pop_front() {
                return Some(m);
            }
            if self.q.closed.load(Ordering::Relaxed) {
                return None;
            }
            self.q.notify.notified().await;
        }
    }
    pub fn try_recv(&mut self) -> Option<Arc<Message>> {
        self.q.q.locked().pop_front()
    }
    /// Messages dropped because this subscriber was too slow.
    pub fn dropped(&self) -> u64 {
        self.q.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Some(h) = self.hub.upgrade() {
            let mut t = h.topics.locked();
            if let Some(tp) = t.get_mut(&self.topic) {
                tp.subs.retain(|(id, _, _)| *id != self.id);
            }
        }
    }
}

impl Hub {
    fn deliver(tp: &mut Topic, mut m: Message, now: Instant) {
        tp.seq += 1;
        if m.seq == 0 {
            m.seq = tp.seq;
        }
        let m = Arc::new(m);
        tp.sent.push_back(now);
        tp.delivered += 1;
        tp.ring.push_back(m.clone());
        while tp.ring.len() > RING {
            tp.ring.pop_front();
        }
        for (_, _, q) in &tp.subs {
            q.push(m.clone());
        }
    }

    fn prune(tp: &mut Topic, now: Instant) {
        while tp.sent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            tp.sent.pop_front();
        }
    }

    fn publish_msg(self: &Arc<Self>, m: Message, opts: PublishOpts) {
        let now = Instant::now();
        let mut schedule = None;
        {
            let mut topics = self.topics.locked();
            let tp = topics.entry(m.topic.clone()).or_default();
            tp.published += 1;
            Self::prune(tp, now);
            if tp.pending.is_none() && tp.sent.len() < RATE_PER_SEC {
                Self::deliver(tp, m, now);
            } else {
                // over the cap: coalesce (last-wins, or append bytes for concat streams)
                match (&mut tp.pending, &m.data) {
                    (Some(Message { data: ChannelData::Binary(p), .. }), ChannelData::Binary(b)) if opts.concat && tp.pending_concat => {
                        p.extend_from_slice(b);
                        tp.coalesced += 1;
                    }
                    (pending, _) => {
                        if pending.is_some() {
                            tp.coalesced += 1;
                        }
                        *pending = Some(m);
                        tp.pending_concat = opts.concat;
                    }
                }
                if !tp.flush_scheduled {
                    tp.flush_scheduled = true;
                    let due = tp.sent.front().map(|t| *t + Duration::from_secs(1)).unwrap_or(now);
                    schedule = Some((tp.pending.as_ref().map(|p| p.topic.clone()).unwrap_or_default(), due));
                }
            }
        }
        if let Some((topic, due)) = schedule {
            let h = self.clone();
            let task = async move {
                let mut due = due;
                loop {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                    let mut topics = h.topics.locked();
                    let Some(tp) = topics.get_mut(&topic) else { return };
                    let now = Instant::now();
                    Self::prune(tp, now);
                    if tp.sent.len() < RATE_PER_SEC {
                        if let Some(p) = tp.pending.take() {
                            Self::deliver(tp, p, now);
                        }
                        tp.flush_scheduled = false;
                        return;
                    }
                    due = tp.sent.front().map(|t| *t + Duration::from_secs(1)).unwrap_or(now);
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(task);
                }
                Err(_) => {
                    std::thread::spawn(move || {
                        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_time().build() {
                            rt.block_on(task);
                        }
                    });
                }
            }
        }
    }

    /// Subscribe; returns the receiver and (when `replay`) the ring buffer, atomically.
    fn subscribe_ring(self: &Arc<Self>, topic: &str, who: &str) -> (Receiver, Vec<Arc<Message>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let q = Arc::new(SubQ::default());
        let mut topics = self.topics.locked();
        let tp = topics.entry(topic.to_string()).or_default();
        let ring = tp.ring.iter().cloned().collect();
        tp.subs.push((id, who.to_string(), q.clone()));
        (Receiver { topic: topic.to_string(), id, q, hub: Arc::downgrade(self) }, ring)
    }

    /// Subscribers to a module's topics (`<m>/*`, `host:<m>/*`) other than the module's own server / host part.
    pub fn outside_subscribers(&self, module: &str) -> usize {
        let (a, b) = (format!("{module}/"), format!("host:{module}/"));
        let (me, host) = (format!("module:{module}"), format!("host:{module}"));
        self.topics
            .locked()
            .iter()
            .filter(|(t, _)| t.starts_with(&a) || t.starts_with(&b))
            .map(|(_, tp)| tp.subs.iter().filter(|(_, w, _)| *w != me && *w != host).count())
            .sum()
    }

    /// The last JSON value of a topic (for `$stream` in snapshot text).
    pub fn last_json(&self, topic: &str) -> Option<Value> {
        self.topics.locked().get(topic)?.ring.iter().rev().find_map(|m| m.json().cloned())
    }

    pub fn ring(&self, topic: &str) -> Vec<Arc<Message>> {
        self.topics.locked().get(topic).map(|t| t.ring.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn stats(&self) -> Vec<Value> {
        let t = self.topics.locked();
        let mut v: Vec<Value> = t
            .iter()
            .map(|(name, tp)| {
                json!({ "topic": name, "subscribers": tp.subs.len(), "published": tp.published, "delivered": tp.delivered, "coalesced": tp.coalesced, "seq": tp.seq, "buffered": tp.ring.len(),
                        "who": tp.subs.iter().map(|(_, w, _)| w.clone()).collect::<HashSet<_>>() })
            })
            .collect();
        v.sort_by(|a, b| a["topic"].as_str().cmp(&b["topic"].as_str()));
        v
    }
}

// ---------- topics and ownership ----------

/// Split and validate `<owner>/<name>`.
pub fn parse_topic(topic: &str) -> Result<(&str, &str), String> {
    let bad = || format!("bad topic \"{topic}\": use \"<owner>/<name>\" (owner = your module id, host:<module> or kernel; name = [A-Za-z0-9._-] segments separated by /)");
    if topic.len() > 200 {
        return Err(bad());
    }
    let (owner, name) = topic.split_once('/').ok_or_else(bad)?;
    let owner_ok = owner == "kernel" || crate::modules::valid_module_id(owner.strip_prefix("host:").unwrap_or(owner));
    let name_ok = !name.is_empty() && name.split('/').all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
    if owner_ok && name_ok { Ok((owner, name)) } else { Err(bad()) }
}

fn trusted(caller: &str) -> bool {
    caller == "agent" || caller == "kernel"
}

pub fn can_publish(caller: &Caller, topic: &str) -> Result<(), String> {
    let (owner, _) = parse_topic(topic)?;
    if trusted(&caller.id) {
        return Ok(());
    }
    let ok = match caller.module() {
        Some(m) => owner == m || (caller.id.starts_with("host:") && owner == format!("host:{m}")),
        None => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{} may not publish on \"{topic}\": a module publishes only on its own topics (\"{}/<name>\")",
            caller.id,
            caller.module().unwrap_or("<module>")
        ))
    }
}

/// Topics another module may subscribe to: manifest `"channels": { "public": ["name" | "<owner>/<name>" | "prefix/*"] }`.
fn is_public(k: &Kernel, owner: &str, topic: &str) -> bool {
    let module = owner.strip_prefix("host:").unwrap_or(owner);
    let g = k.lock();
    let Some(list) = g.modules.get(module).and_then(|m| m.manifest.as_ref()).and_then(|mf| mf.raw.pointer("/channels/public")).and_then(Value::as_array).cloned() else {
        return false;
    };
    drop(g);
    list.iter().filter_map(Value::as_str).any(|e| {
        let full = if e.contains('/') && parse_topic(e).is_ok_and(|(o, _)| o == owner) { e.to_string() } else { format!("{owner}/{e}") };
        match full.strip_suffix('*') {
            Some(prefix) => topic.starts_with(prefix),
            None => full == topic,
        }
    })
}

pub fn can_subscribe(k: &Kernel, caller: &Caller, topic: &str) -> Result<(), String> {
    let (owner, _) = parse_topic(topic)?;
    if trusted(&caller.id) || caller.id == "user" || owner == "kernel" {
        return Ok(());
    }
    if let Some(m) = caller.module()
        && (owner == m || owner == format!("host:{m}"))
    {
        return Ok(());
    }
    if is_public(k, owner, topic) {
        return Ok(());
    }
    Err(format!(
        "{} may not subscribe to \"{topic}\": the owner must list it in its openapp.json \"channels\": {{ \"public\": [\"…\"] }}",
        caller.id
    ))
}

// ---------- Rust API ----------

/// Publish as the kernel (no ownership check). For kernel code and other primitives.
pub fn publish(k: &Kernel, topic: &str, data: ChannelData) {
    publish_with(k, topic, data, "kernel", PublishOpts::default());
}

pub fn publish_with(k: &Kernel, topic: &str, data: ChannelData, from: &str, opts: PublishOpts) {
    let m = Message { topic: topic.to_string(), seq: opts.seq.unwrap_or(0), ts: crate::util::now_ms(), from: from.to_string(), data };
    hub(k).publish_msg(m, opts);
}

/// Publish with the ownership check of `caller`.
pub fn publish_as(k: &Kernel, caller: &Caller, topic: &str, data: ChannelData, opts: PublishOpts) -> Result<(), String> {
    can_publish(caller, topic)?;
    let size = match &data {
        ChannelData::Json(v) => v.to_string().len(),
        ChannelData::Binary(b) => b.len(),
    };
    if size > MAX_BYTES {
        return Err(format!("message too large ({size} bytes, max {MAX_BYTES})"));
    }
    publish_with(k, topic, data, &caller.id, opts);
    Ok(())
}

/// Subscribe as the kernel. `replay`: the ring buffer (last 64) is queued first.
pub fn subscribe(k: &Kernel, topic: &str, replay: bool) -> Receiver {
    let (rx, ring) = hub(k).subscribe_ring(topic, "kernel");
    if replay {
        for m in ring {
            rx.q.push(m);
        }
    }
    rx
}

/// Subscribe with the permission check of `caller` (and G9 onChannel activation of the owner).
pub fn subscribe_as(k: &Kernel, caller: &Caller, topic: &str, replay: bool) -> Result<Receiver, String> {
    let (rx, ring) = subscribe_as_ring(k, caller, topic)?;
    if replay {
        for m in ring {
            rx.q.push(m);
        }
    }
    Ok(rx)
}

/// Like [`subscribe_as`], returning the ring buffer separately (taken atomically with the subscription).
pub fn subscribe_as_ring(k: &Kernel, caller: &Caller, topic: &str) -> Result<(Receiver, Vec<Arc<Message>>), String> {
    can_subscribe(k, caller, topic)?;
    let (owner, _) = parse_topic(topic)?;
    let r = hub(k).subscribe_ring(topic, &caller.id);
    if owner != "kernel" {
        super::lifecycle::on_channel(k, owner.strip_prefix("host:").unwrap_or(owner));
    }
    Ok(r)
}

/// `{ data }` | `{ b64 }` → ChannelData.
pub fn data_of(p: &Value) -> Result<ChannelData, String> {
    if let Some(b) = p.get("b64").and_then(Value::as_str) {
        return base64::engine::general_purpose::STANDARD.decode(b).map(ChannelData::Binary).map_err(|e| format!("b64 is not base64: {e}"));
    }
    match p.get("data") {
        Some(v) => Ok(ChannelData::Json(v.clone())),
        None => Err("publish needs \"data\" (JSON) or \"b64\" (base64 bytes)".into()),
    }
}

fn opts_of(p: &Value) -> PublishOpts {
    PublishOpts { concat: p.get("concat").and_then(Value::as_bool).unwrap_or(false), seq: p.get("seq").and_then(Value::as_u64) }
}

fn publish_params(k: &Kernel, p: &Value, caller: &Caller) -> Result<Value, String> {
    let topic = arg(p, "topic").ok_or("channel.publish needs \"topic\"")?;
    publish_as(k, caller, topic, data_of(p)?, opts_of(p))?;
    if let Some(m) = caller.module() {
        super::lifecycle::touch(k, m);
    }
    Ok(json!({ "ok": true, "topic": topic }))
}

// ---------- views (Tauri commands; called from lib.rs) ----------

/// `channel_subscribe { placementId, topic }` → `{ sub, replay: [message] }`; live messages arrive as `oa://channel`
/// with `sub` (one kernel subscription per call, so the shell routes by `sub`).
pub fn view_subscribe(k: &Arc<Kernel>, placement: &str, topic: &str) -> Result<Value, String> {
    let caller = Caller::for_placement(k, placement);
    let (mut rx, ring) = subscribe_as_ring(k, &caller, topic)?;
    let ring: Vec<Value> = ring.iter().map(|m| m.to_json()).collect();
    let id = hub(k).next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let k2 = k.clone();
    let task = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let mut v = m.to_json();
            v["sub"] = json!(id);
            k2.sink.emit("oa://channel", v);
        }
    });
    hub(k).shell.locked().insert(id, (topic.to_string(), task.abort_handle()));
    Ok(json!({ "sub": id, "topic": topic, "replay": ring }))
}

pub fn view_unsubscribe(k: &Kernel, sub: u64) {
    if let Some((_, h)) = hub(k).shell.locked().remove(&sub) {
        h.abort();
    }
}

pub fn view_publish(k: &Arc<Kernel>, placement: &str, p: &Value) -> Result<Value, String> {
    let caller = Caller::for_placement(k, placement);
    publish_params(k, p, &caller)
}

// ---------- host modules (JSON-RPC notifications) ----------

fn attach_host(k: &Kernel, module: &str) {
    let Some(sc) = k.lock().modules.get(module).and_then(|m| m.host.clone()) else { return };
    // a reload replaces the sidecar: drop the old forwarders
    if let Some(old) = hub(k).host.locked().remove(module) {
        old.values().for_each(|h| h.abort());
    }
    let Some(ka) = k.arc() else { return };
    let weak = Arc::downgrade(&ka);
    let scw = Arc::downgrade(&sc);
    let module = module.to_string();
    sc.set_on_notify(Arc::new(move |method, params| {
        let (Some(k), Some(sc)) = (weak.upgrade(), scw.upgrade()) else { return };
        let caller = Caller::new(format!("host:{module}"));
        let topic = arg(&params, "topic").unwrap_or("").to_string();
        match method {
            "channel.publish" => {
                if let Err(e) = publish_params(&k, &params, &caller) {
                    log::warn!("host module {module}: channel.publish: {e}");
                    sc.notify("channel.error", json!({ "topic": topic, "error": e }));
                }
            }
            "channel.subscribe" => match subscribe_as(&k, &caller, &topic, params.get("replay").and_then(Value::as_bool).unwrap_or(true)) {
                Ok(mut rx) => {
                    let scw = Arc::downgrade(&sc);
                    let task = k.arc().map(|_| {
                        tokio::spawn(async move {
                            while let Some(m) = rx.recv().await {
                                match scw.upgrade() {
                                    Some(sc) if sc.notify("channel.message", m.to_json()) => {}
                                    _ => break,
                                }
                            }
                        })
                    });
                    if let Some(t) = task {
                        hub(&k).host.locked().entry(module.clone()).or_default().insert(topic, t.abort_handle());
                    }
                }
                Err(e) => {
                    sc.notify("channel.error", json!({ "topic": topic, "error": e }));
                }
            },
            "channel.unsubscribe" => {
                if let Some(h) = hub(&k).host.locked().get_mut(&module).and_then(|m| m.remove(&topic)) {
                    h.abort();
                }
            }
            other => log::debug!("host module {module}: notification {other} ignored"),
        }
    }));
}

// ---------- WebSocket for module servers ----------

async fn ws_route(
    axum::extract::State(k): axum::extract::State<Arc<Kernel>>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let bearer = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(String::from);
    let token = bearer.or_else(|| q.get("token").cloned()).unwrap_or_default();
    let Some(caller) = k.host_caller(&token) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "missing or wrong token (use $OPENAPP_CHANNEL_URL as given, or Authorization: Bearer $OPENAPP_HOST_TOKEN)").into_response();
    };
    ws.max_message_size(MAX_BYTES + 4096).on_upgrade(move |socket| ws_session(k, Caller::new(caller), socket))
}

/// Binary frame: `<JSON header>\n<payload>`; header `{ "topic", "seq"? }` (in: also `"op": "publish"`, `"concat"?`).
fn split_binary(b: &[u8]) -> Option<(Value, &[u8])> {
    let nl = b.iter().position(|&c| c == b'\n')?;
    Some((serde_json::from_slice(&b[..nl]).ok()?, &b[nl + 1..]))
}

fn to_ws(m: &Message) -> WsMsg {
    match &m.data {
        ChannelData::Json(d) => WsMsg::Text(json!({ "op": "message", "topic": m.topic, "seq": m.seq, "ts": m.ts, "data": d }).to_string().into()),
        ChannelData::Binary(b) => {
            let mut out = json!({ "op": "message", "topic": m.topic, "seq": m.seq, "ts": m.ts }).to_string().into_bytes();
            out.push(b'\n');
            out.extend_from_slice(b);
            WsMsg::Binary(out.into())
        }
    }
}

async fn ws_session(k: Arc<Kernel>, caller: Caller, mut socket: WebSocket) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WsMsg>(1024);
    let module = caller.module().map(String::from);
    let mut subs: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let reply = |tx: &tokio::sync::mpsc::Sender<WsMsg>, v: Value| {
        let _ = tx.try_send(WsMsg::Text(v.to_string().into()));
    };
    loop {
        let frame = tokio::select! {
            f = socket.recv() => match f {
                Some(Ok(f)) => f,
                _ => break,
            },
            out = rx.recv() => {
                match out {
                    Some(m) => {
                        if socket.send(m).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    None => break,
                }
            }
        };
        let (op, p, bin): (String, Value, Option<Vec<u8>>) = match frame {
            WsMsg::Text(t) => match serde_json::from_str::<Value>(&t) {
                Ok(v) => (arg(&v, "op").unwrap_or("").to_string(), v, None),
                Err(e) => {
                    reply(&tx, json!({ "op": "error", "error": format!("frame is not JSON: {e}") }));
                    continue;
                }
            },
            WsMsg::Binary(b) => match split_binary(&b) {
                Some((h, payload)) => ("publish".into(), h, Some(payload.to_vec())),
                None => {
                    reply(&tx, json!({ "op": "error", "error": "binary frame must be <JSON header>\\n<bytes>" }));
                    continue;
                }
            },
            WsMsg::Close(_) => break,
            _ => continue,
        };
        let topic = arg(&p, "topic").unwrap_or("").to_string();
        match op.as_str() {
            "publish" => {
                let r = match bin {
                    Some(b) => publish_as(&k, &caller, &topic, ChannelData::Binary(b), opts_of(&p)),
                    None => data_of(&p).and_then(|d| publish_as(&k, &caller, &topic, d, opts_of(&p))),
                };
                match r {
                    Ok(()) => {
                        if let Some(m) = &module {
                            super::lifecycle::touch(&k, m);
                        }
                    }
                    Err(e) => reply(&tx, json!({ "op": "error", "topic": topic, "error": e })),
                }
            }
            "subscribe" => {
                if subs.contains_key(&topic) {
                    continue;
                }
                match subscribe_as(&k, &caller, &topic, p.get("replay").and_then(Value::as_bool).unwrap_or(true)) {
                    Ok(mut r) => {
                        let tx2 = tx.clone();
                        subs.insert(
                            topic.clone(),
                            tokio::spawn(async move {
                                while let Some(m) = r.recv().await {
                                    if tx2.send(to_ws(&m)).await.is_err() {
                                        break;
                                    }
                                }
                            }),
                        );
                        if let Some(m) = &module {
                            *hub(&k).ws.locked().entry(m.clone()).or_default() += 1;
                        }
                        reply(&tx, json!({ "op": "subscribed", "topic": topic }));
                    }
                    Err(e) => reply(&tx, json!({ "op": "error", "topic": topic, "error": e })),
                }
            }
            "unsubscribe" => {
                if let Some(h) = subs.remove(&topic) {
                    h.abort();
                    if let Some(m) = &module {
                        let h = hub(&k);
                        let mut ws = h.ws.locked();
                        let n = ws.entry(m.clone()).or_default();
                        *n = n.saturating_sub(1);
                    }
                }
            }
            "ping" => reply(&tx, json!({ "op": "pong" })),
            other => reply(&tx, json!({ "op": "error", "error": format!("unknown op \"{other}\" (publish, subscribe, unsubscribe, ping)") })),
        }
    }
    if let Some(m) = &module {
        let h = hub(&k);
        let mut ws = h.ws.locked();
        let n = ws.entry(m.clone()).or_default();
        *n = n.saturating_sub(subs.len());
    }
    for (_, h) in subs {
        h.abort();
    }
}

/// `$OPENAPP_CHANNEL_URL` for a module server.
pub fn channel_url(k: &Kernel, module: &str, in_vm: bool) -> String {
    let port = if in_vm { crate::vm::MCP_PORT } else { k.mcp.get().map(|e| e.port).unwrap_or(0) };
    format!("ws://127.0.0.1:{port}/channel?token={}", k.host_token(&format!("module:{module}")))
}

// ---------- kernel/* topics ----------

fn kernel_publish_if_changed(k: &Kernel, topic: &'static str, v: Value) {
    let s = v.to_string();
    {
        let h = hub(k);
        let mut last = h.last_kernel.locked();
        if last.get(topic) == Some(&s) {
            return;
        }
        last.insert(topic, s);
    }
    publish(k, topic, ChannelData::Json(v));
}

fn on_event(k: &Kernel, ev: &Event) {
    match ev {
        Event::Snapshot(s) => {
            kernel_publish_if_changed(k, "kernel/sessions", json!({ "active": s.active_session, "mode": s.active_mode, "sessions": s.sessions }));
            kernel_publish_if_changed(k, "kernel/agent", json!(s.agent));
        }
        Event::Notify(t) => publish(k, "kernel/notify", ChannelData::Json(json!({ "text": t }))),
        Event::ModuleLoaded(id) => attach_host(k, id),
        Event::ModuleUnloaded(id) => {
            if let Some(old) = hub(k).host.locked().remove(*id) {
                old.values().for_each(|h| h.abort());
            }
        }
        _ => {}
    }
}

// ---------- $stream in snapshot text ----------

/// Resolve `{ "$stream": topic, "path"?: "a.b", "default"? }` against the last message (shell semantics).
pub fn resolve_stream(binding: &Value, last: Option<&Value>) -> Value {
    let default = binding.get("default").cloned().unwrap_or(Value::Null);
    let Some(mut v) = last else { return default };
    if let Some(path) = binding.get("path").and_then(Value::as_str).filter(|p| !p.is_empty()) {
        for seg in path.split('.') {
            let next = match v {
                Value::Object(o) => o.get(seg),
                Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
                _ => None,
            };
            match next {
                Some(n) => v = n,
                None => return default,
            }
        }
    }
    if v.is_null() { default } else { v.clone() }
}

pub fn primitive() -> Primitive {
    let publish_h = handler(|k, p, c| async move { publish_params(&k, &p, &c) });
    let read_h = handler(|k, p, c| async move {
        let topic = arg(&p, "topic").ok_or("channel.read needs \"topic\"")?;
        can_subscribe(&k, &c, topic)?;
        let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(RING as u64) as usize;
        let ring = hub(&k).ring(topic);
        let msgs: Vec<Value> = ring.iter().skip(ring.len().saturating_sub(limit)).map(|m| m.to_json()).collect();
        Ok::<_, String>(json!({ "topic": topic, "messages": msgs }))
    });
    Primitive {
        name: "channels",
        purpose: "Live pub/sub topics between module servers (VM), host modules and views: JSON or binary, last 64 replayed to late subscribers, 120 msg/s per topic (excess coalesced, last-wins).",
        example: "server: ws = new WebSocket(process.env.OPENAPP_CHANNEL_URL); ws.send(JSON.stringify({op:\"publish\", topic:\"mail/new\", data:{count:3}})) · spec: { \"type\": \"badge\", \"label\": { \"$stream\": \"mail/new\", \"path\": \"count\", \"default\": 0 } } · html: openapp.subscribe(\"mail/new\", d => …)",
        class: Class::Free,
        actions: vec![
            Action { name: "channel.publish", args: "{ topic, data | b64 } — views publish on their module's topics (\"<module>/<name>\")", handler: publish_h.clone() },
        ],
        tools: vec![
            McpTool {
                name: "openapp_channel_publish",
                description: "Publish a message on a live channel topic \"<owner>/<name>\" (data = JSON, or b64 = base64 bytes). Views bound with $stream and subscribers update at once.",
                schema: json!({ "type": "object", "properties": { "topic": { "type": "string" }, "data": {}, "b64": { "type": "string" } }, "required": ["topic"] }),
                handler: publish_h.clone(),
            },
            McpTool {
                name: "openapp_channel_read",
                description: "The recent messages of a live channel topic (ring buffer, last 64), to check what a module publishes. Also lists nothing if no one published yet.",
                schema: json!({ "type": "object", "properties": { "topic": { "type": "string" }, "limit": { "type": "integer" } }, "required": ["topic"] }),
                handler: read_h.clone(),
            },
        ],
        endpoints: vec![
            Endpoint { method: "channel.publish", args: "{ topic, data | b64, seq?, concat? } — own topics \"<module>/<name>\" only", handler: publish_h },
            Endpoint { method: "channel.read", args: "{ topic, limit? } → { messages } (ring buffer)", handler: read_h },
        ],
        data: vec![DataSource {
            name: "channels",
            doc: "list items [{ id: topic, title: topic, subtitle: \"<n> subscribers · <delivered> sent · <coalesced> coalesced\", icon: \"radio\" }] — live topics",
            get: data_fn(|k| {
                Value::Array(
                    hub(k)
                        .stats()
                        .into_iter()
                        .map(|s| json!({ "id": s["topic"], "title": s["topic"], "subtitle": format!("{} subscribers · {} sent · {} coalesced", s["subscribers"], s["delivered"], s["coalesced"]), "icon": "radio" }))
                        .collect(),
                )
            }),
        }],
        commands: vec![],
        slash: vec![],
        env: Some(Arc::new(|k, module, in_vm| vec![("OPENAPP_CHANNEL_URL".into(), channel_url(k, module, in_vm))])),
        router: Some(|k| axum::Router::new().route("/channel", axum::routing::get(ws_route)).with_state(k)),
        on_event: Some(Arc::new(on_event)),
        describe_keys: vec![(
            "channels",
            data_fn(|k| {
                json!({
                    "topics": hub(k).stats(),
                    "kernel_topics": KERNEL_TOPICS,
                    "limits": { "ring": RING, "rate_per_topic_per_sec": RATE_PER_SEC, "subscriber_queue": SUB_QUEUE, "max_bytes": MAX_BYTES },
                    "rules": "publish only on your own topics \"<module>/<name>\"; other modules may subscribe only to topics the owner lists in openapp.json \"channels\": { \"public\": [\"<name>\" | \"<prefix>/*\"] }; kernel/* is readable by all",
                    "module_servers": "WebSocket $OPENAPP_CHANNEL_URL: send {op:\"subscribe\", topic, replay?} | {op:\"publish\", topic, data} | {op:\"unsubscribe\", topic}; receive {op:\"message\", topic, seq, ts, data}; binary frames = <JSON header {topic, seq}>\\n<bytes>. Or POST $OPENAPP_HOST_URL/channel.publish {topic, data|b64}.",
                    "host_modules": "JSON-RPC notifications on stdout: {\"jsonrpc\":\"2.0\",\"method\":\"channel.publish\",\"params\":{topic, data|b64}} · channel.subscribe {topic} → you receive {\"method\":\"channel.message\",\"params\":{topic, seq, ts, data|b64}}",
                    "views": "spec: any prop { \"$stream\": topic, \"path\"?: \"a.b\", \"default\"? }; html: openapp.subscribe(topic, (data, msg) => …) → unsubscribe, openapp.publish(topic, data)"
                })
            }),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_and_ownership() {
        assert!(parse_topic("mail/new").is_ok());
        assert!(parse_topic("host:daw/meters/l").is_ok());
        assert!(parse_topic("kernel/sessions").is_ok());
        assert!(parse_topic("Mail/new").is_err() && parse_topic("mail").is_err() && parse_topic("mail/").is_err() && parse_topic("mail/a b").is_err());
        assert!(can_publish(&Caller::new("module:mail"), "mail/new").is_ok());
        assert!(can_publish(&Caller::new("view:mail"), "mail/input").is_ok());
        assert!(can_publish(&Caller::new("host:daw"), "host:daw/meters").is_ok());
        assert!(can_publish(&Caller::new("module:evil"), "mail/new").is_err());
        assert!(can_publish(&Caller::new("module:mail"), "kernel/notify").is_err());
        assert!(can_publish(&Caller::new("agent"), "mail/new").is_ok());
    }

    #[test]
    fn stream_resolver() {
        let last = json!({ "a": { "b": [10, 20] }, "n": null });
        assert_eq!(resolve_stream(&json!({ "$stream": "m/t", "path": "a.b.1" }), Some(&last)), json!(20));
        assert_eq!(resolve_stream(&json!({ "$stream": "m/t", "path": "a.x", "default": 0 }), Some(&last)), json!(0));
        assert_eq!(resolve_stream(&json!({ "$stream": "m/t", "path": "n", "default": "-" }), Some(&last)), json!("-"));
        assert_eq!(resolve_stream(&json!({ "$stream": "m/t", "default": 5 }), None), json!(5));
        assert_eq!(resolve_stream(&json!({ "$stream": "m/t" }), Some(&json!(7))), json!(7));
    }

    #[tokio::test]
    async fn ring_rate_and_drop_oldest() {
        let hub = Arc::new(Hub::default());
        let mk = |i: u64| Message { topic: "m/t".into(), seq: 0, ts: 0, from: "kernel".into(), data: ChannelData::Json(json!(i)) };
        let (mut rx, _) = hub.subscribe_ring("m/t", "test");
        for i in 0..1000 {
            hub.publish_msg(mk(i), PublishOpts::default());
        }
        // first 120 delivered at once, the rest coalesced into one pending (last-wins)
        let mut got = Vec::new();
        while let Some(m) = rx.try_recv() {
            got.push(m.json().cloned().unwrap());
        }
        assert_eq!(got.len(), RATE_PER_SEC);
        assert_eq!(hub.ring("m/t").len(), RING);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let last = rx.try_recv().expect("coalesced flush after the window");
        assert_eq!(last.json(), Some(&json!(999)));
        // late subscriber gets the ring (last 64), ending with the flushed message
        let (_rx2, ring) = hub.subscribe_ring("m/t", "late");
        assert_eq!(ring.len(), RING);
        assert_eq!(ring.last().unwrap().json(), Some(&json!(999)));
        // drop-oldest per subscriber
        let q = SubQ::default();
        for i in 0..(SUB_QUEUE as u64 + 10) {
            q.push(Arc::new(mk(i)));
        }
        assert_eq!(q.dropped.load(Ordering::Relaxed), 10);
        assert_eq!(q.q.lock().unwrap().front().unwrap().json(), Some(&json!(10)));
    }

    #[tokio::test]
    async fn concat_binary_while_coalescing() {
        let hub = Arc::new(Hub::default());
        let (mut rx, _) = hub.subscribe_ring("p/out", "test");
        for i in 0..(RATE_PER_SEC + 3) {
            let m = Message { topic: "p/out".into(), seq: 0, ts: 0, from: "k".into(), data: ChannelData::Binary(vec![i as u8]) };
            hub.publish_msg(m, PublishOpts { concat: true, seq: None });
        }
        while rx.try_recv().is_some() {}
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let m = rx.try_recv().unwrap();
        assert_eq!(m.data, ChannelData::Binary(vec![120, 121, 122]));
    }
}

thread_local! {
    static SNAP_HUB: std::cell::RefCell<Option<Arc<Hub>>> = const { std::cell::RefCell::new(None) };
}

/// While alive, [`resolve_tree_tls`] on this thread resolves `$stream` against this kernel's hub.
pub struct SnapshotStreams;

impl SnapshotStreams {
    pub fn enter(k: &Kernel) -> Self {
        let h = hub(k);
        SNAP_HUB.with(|c| *c.borrow_mut() = Some(h));
        SnapshotStreams
    }
}

impl Drop for SnapshotStreams {
    fn drop(&mut self) {
        SNAP_HUB.with(|c| *c.borrow_mut() = None);
    }
}

/// `$stream` bindings → `"<value>"` text for `openapp_snapshot` (tree unchanged outside a [`SnapshotStreams`]).
pub fn resolve_tree_tls(tree: &Value) -> Value {
    let Some(h) = SNAP_HUB.with(|c| c.borrow().clone()) else { return tree.clone() };
    fn walk(h: &Hub, v: &Value) -> Value {
        match v {
            Value::Object(o) if o.get("$stream").is_some_and(Value::is_string) => {
                let r = resolve_stream(v, h.last_json(o["$stream"].as_str().unwrap_or("")).as_ref());
                match r {
                    Value::String(_) | Value::Number(_) | Value::Bool(_) => r,
                    Value::Null => json!(format!("{{$stream {}: no message yet}}", o["$stream"].as_str().unwrap_or(""))),
                    other => other,
                }
            }
            Value::Object(o) => Value::Object(o.iter().map(|(a, b)| (a.clone(), walk(h, b))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(h, x)).collect()),
            v => v.clone(),
        }
    }
    walk(&h, tree)
}
