# Live channels (push updates, streams, view input)

Use a channel when data changes by itself (new mail, meters, progress, a playhead) instead of polling a tool.
Topics are `<module>/<name>`; you publish only on your own module's topics. The last 64 messages are replayed to a late
subscriber; more than 120 messages/s per topic are coalesced (the newest wins), so send state, not deltas.
- Spec view: any prop `{ "$stream": "mail/new", "path": "count", "default": 0 }` (re-renders on each message; `path`
  digs into the JSON). `openapp_snapshot` shows the current value.
- HTML view: `const off = openapp.subscribe("mail/new", (data, msg) => …)` (binary → `Uint8Array`);
  `openapp.publish("mail/keys", { key: "j" })` sends view input to your server.
- Server: `$OPENAPP_CHANNEL_URL` is a WebSocket (token included); `POST $OPENAPP_HOST_URL/channel.publish` works too.

```ts
const ws = new WebSocket(process.env.OPENAPP_CHANNEL_URL!);   // Node 24: global WebSocket
ws.onopen = () => ws.send(JSON.stringify({ op: "subscribe", topic: "mail/keys" }));
ws.onmessage = (e) => { const m = JSON.parse(String(e.data)); if (m.op === "message") console.error("key", m.data); };
setInterval(() => ws.send(JSON.stringify({ op: "publish", topic: "mail/new", data: { count: 3 } })), 1000);
```
Other modules may subscribe only to topics you list: `"channels": { "public": ["new", "meters/*"] }`.
`kernel/sessions`, `kernel/agent`, `kernel/jobs`, `kernel/notify` are readable by all. Host modules: JSON-RPC
notifications `channel.publish {topic, data}` / `channel.subscribe {topic}` on stdout; messages come back as
`channel.message`. Debug with `openapp_channel_read {topic}` and `openapp_channel_publish {topic, data}`.
