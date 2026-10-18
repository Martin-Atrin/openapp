//! Line-framed JSON peers on a child's stdio: the agent harnesses (ACP and Codex app-server speak JSON-RPC 2.0,
//! Pi its own RPC records) and the §18 host sidecars.
//!
//! - [`writer`] / [`read_lines`]: one JSON value per LF-terminated line (strict LF framing: U+2028/U+2029 are valid
//!   inside JSON strings), non-JSON lines go to the stderr tail.
//! - [`Peer`]: the pending map (request id → waiter) with timeouts.
//! - [`JsonRpc`]: request / notify / respond / error mapping on top of a [`Peer`].

use crate::modules::{self, Tail};
use crate::util::MutexExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Spawn the writer task: each queued string goes out as one line.
pub fn writer<W: AsyncWrite + Unpin + Send + 'static>(mut w: W) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
    });
    tx
}

/// Read JSON lines until EOF; `f` gets every JSON value, other non-blank lines go to `tail`.
pub async fn read_lines<R: AsyncRead + Unpin>(r: R, tail: &Tail, mut f: impl FnMut(Value)) {
    let mut r = tokio::io::BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(v) => f(v),
            Err(_) => modules::push_tail(tail, format!("[stdout, not JSON] {}", String::from_utf8_lossy(line).trim())),
        }
    }
}

pub enum Wait {
    /// the peer went away (or answered every waiter with an exit error)
    Closed,
    Timeout,
}

/// Outgoing line queue + pending replies by id.
pub struct Peer {
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    next: AtomicU64,
}

impl Peer {
    pub fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self { tx, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1) }
    }

    /// Queue one message (false when the writer is gone).
    pub fn send(&self, v: &Value) -> bool {
        self.tx.send(v.to_string()).is_ok()
    }

    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Send `msg` and wait for the reply registered under `key`.
    pub async fn call(&self, key: String, msg: &Value, timeout: Duration) -> Result<Value, Wait> {
        let (tx, rx) = oneshot::channel();
        self.pending.locked().insert(key.clone(), tx);
        if !self.send(msg) {
            self.pending.locked().remove(&key);
            return Err(Wait::Closed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(Wait::Closed),
            Err(_) => {
                self.pending.locked().remove(&key);
                Err(Wait::Timeout)
            }
        }
    }

    /// Hand a reply to its waiter (false: nobody waits for `key`).
    pub fn resolve(&self, key: &str, v: Value) -> bool {
        match self.pending.locked().remove(key) {
            Some(tx) => {
                let _ = tx.send(v);
                true
            }
            None => false,
        }
    }

    /// Answer every waiter with `v` (e.g. an exit error).
    pub fn fail_all(&self, v: Value) {
        let p: Vec<_> = self.pending.locked().drain().collect();
        for (_, tx) in p {
            let _ = tx.send(v.clone());
        }
    }

    /// Drop every waiter (they see [`Wait::Closed`]).
    pub fn clear(&self) {
        self.pending.locked().clear();
    }
}

/// `message (code N): data` of a JSON-RPC error object.
pub fn err_text(e: &Value) -> String {
    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
    let code = e.get("code").map(|c| c.to_string()).unwrap_or_default();
    match e.get("data") {
        Some(d) if !d.is_null() => format!("{msg} (code {code}): {}", d.as_str().map(String::from).unwrap_or_else(|| d.to_string())),
        _ => format!("{msg} (code {code})"),
    }
}

pub enum RpcErr {
    /// the JSON-RPC `error` object
    Remote(Value),
    Closed,
    Timeout,
}

/// A message from the other side that is not a reply.
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
}

/// JSON-RPC 2.0 over a [`Peer`]. Codex app-server omits the `"jsonrpc"` member (`tagged: false`).
pub struct JsonRpc {
    pub peer: Peer,
    tagged: bool,
    /// who answers, for "… closed the connection"
    name: String,
}

impl JsonRpc {
    pub fn new(tx: mpsc::UnboundedSender<String>, tagged: bool, name: &str) -> Self {
        Self { peer: Peer::new(tx), tagged, name: name.into() }
    }

    fn msg(&self, mut v: Value) -> Value {
        if self.tagged {
            v["jsonrpc"] = json!("2.0");
        }
        v
    }

    pub async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcErr> {
        let id = self.peer.next_id();
        let msg = self.msg(json!({ "id": id, "method": method, "params": params }));
        match self.peer.call(id.to_string(), &msg, timeout).await {
            Ok(r) => match r.get("error") {
                Some(e) if !e.is_null() => Err(RpcErr::Remote(e.clone())),
                _ => Ok(r.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(Wait::Closed) => Err(RpcErr::Closed),
            Err(Wait::Timeout) => Err(RpcErr::Timeout),
        }
    }

    /// A request with the errors as text.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.call(method, params, timeout).await.map_err(|e| match e {
            RpcErr::Remote(e) => err_text(&e),
            RpcErr::Closed => format!("{} closed the connection", self.name),
            RpcErr::Timeout => format!("{method} timed out after {}s", timeout.as_secs()),
        })
    }

    /// A notification (`params` None: no `params` member).
    pub fn notify(&self, method: &str, params: Option<Value>) -> bool {
        let mut v = json!({ "method": method });
        if let Some(p) = params {
            v["params"] = p;
        }
        self.peer.send(&self.msg(v))
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.peer.send(&self.msg(json!({ "id": id, "result": result })));
    }

    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        self.peer.send(&self.msg(json!({ "id": id, "error": { "code": code, "message": message } })));
    }

    /// Every pending request fails with `{ code: -32000, message }`.
    pub fn fail_pending(&self, message: &str) {
        self.peer.fail_all(json!({ "error": { "code": -32000, "message": message } }));
    }

    /// Route one message: a reply resolves its request (None); requests and notifications are returned.
    pub fn route(&self, v: Value) -> Option<Incoming> {
        let method = v.get("method").and_then(Value::as_str).map(String::from);
        let id = v.get("id").cloned().filter(|i| !i.is_null());
        match (method, id) {
            (Some(method), Some(id)) => Some(Incoming::Request { id, method, params: v.get("params").cloned().unwrap_or(Value::Null) }),
            (Some(method), None) => Some(Incoming::Notification { method, params: v.get("params").cloned().unwrap_or(Value::Null) }),
            (None, Some(id)) => {
                if let Some(n) = id.as_u64() {
                    self.peer.resolve(&n.to_string(), v);
                }
                None
            }
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_reply_timeout_and_close() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let rpc = std::sync::Arc::new(JsonRpc::new(tx, true, "test peer"));
        let r2 = rpc.clone();
        let echo = tokio::spawn(async move {
            let line = rx.recv().await.unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
            assert!(r2.route(json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "ok": true } })).is_none());
            let line = rx.recv().await.unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            r2.route(json!({ "id": v["id"], "error": { "code": -32601, "message": "unknown method", "data": "x" } }));
            rx
        });
        assert_eq!(rpc.request("a", json!({}), Duration::from_secs(5)).await.unwrap(), json!({ "ok": true }));
        assert_eq!(rpc.request("b", json!({}), Duration::from_secs(5)).await.unwrap_err(), "unknown method (code -32601): x");
        let _rx = echo.await.unwrap();
        assert_eq!(rpc.request("c", json!({}), Duration::from_millis(50)).await.unwrap_err(), "c timed out after 0s");
        let r3 = rpc.clone();
        let waiting = tokio::spawn(async move { r3.request("d", json!({}), Duration::from_secs(5)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        rpc.fail_pending("test peer exited");
        assert_eq!(waiting.await.unwrap().unwrap_err(), "test peer exited (code -32000)");
        assert!(matches!(rpc.route(json!({ "method": "n", "params": 1 })), Some(Incoming::Notification { .. })));
        assert!(matches!(rpc.route(json!({ "id": 7, "method": "r" })), Some(Incoming::Request { .. })));
    }

    #[tokio::test]
    async fn lines_are_lf_framed() {
        let tail: Tail = std::sync::Arc::new(Mutex::new(Default::default()));
        let data = "{\"a\":\"x\u{2028}y\"}\r\n\nnot json\n{\"b\":2}".as_bytes();
        let mut got = Vec::new();
        read_lines(data, &tail, |v| got.push(v)).await;
        assert_eq!(got, vec![json!({ "a": "x\u{2028}y" }), json!({ "b": 2 })]);
        assert_eq!(modules::tail_lines(&tail, 5), vec!["[stdout, not JSON] not json".to_string()]);
    }
}
