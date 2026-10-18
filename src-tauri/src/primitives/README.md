# Primitives (contract §21)

Each primitive is one file `primitives/<name>.rs` with `pub fn primitive() -> Primitive`. Register it with
one line in `build()` in `mod.rs`. The kernel consults the registry after its built-ins:

- `actions`: host actions `{ "host": name, … }` from views (`ui_action`, awaited, result returned), chrome
  (fire-and-forget) and the agent (`openapp_host_action`). Handler: `handler(|k, params, caller| async move { … })`.
- `tools`: MCP tools for the agent (name, description, JSON schema, handler; caller `agent`).
- `endpoints`: `POST /host/<method>` for module servers (caller `module:<id>` from the bearer token) and `host_call`.
- `data`: host data sources; the shell resolves `{ "$host": name }` from `snapshot.hostData[name]`.
- `commands`: Tauri `primitive_command { name, args, placementId? }`; `slash`: chat commands (`/safe`).
- `env`: extra env for module servers (`OPENAPP_CHANNEL_URL`, …); `router`: extra axum routes on the kernel
  HTTP server (the VM reaches it at 127.0.0.1:7801); `on_event`: Started, ModuleLoaded/Unloaded, Snapshot, Notify, Shutdown.
- `name/purpose/example/class` (Free | Granted | Confirmed) → `openapp_describe → primitives`; `describe_keys` add top-level keys.
- Per-kernel state: `k.prims.get::<MyState>()` (any `Default + Send + Sync` type). `k.arc()` gives an `Arc<Kernel>`.
- Errors: handlers return `Result<Value, String>` or `Result<Value, HostError>`; the `HostError` code
  (`bad_request` | `forbidden` | `not_found` | `conflict` | `upstream` | `internal`) is the HTTP status of
  `POST /host/<method>`; a plain string is `upstream` (502). Actions and MCP tools see only the message.
- Consent: `crate::consent::module_grant(k, kind, module, title, text)` (Granted, remembered per module),
  `consent::granted` (any target), `consent::ask` (a card every time). Decisions live in `k.grants`
  (`<state>/grants.json`, one shape `{ allowed, scope, ts, … }`, atomic writes).
- Kernel built-ins are not registry primitives: `chrome.rs`, `audio.rs` and `connect.rs` hold `Builtin` tables
  (name, describe args, required view keys, a synchronous `run`). They come first; `host_actions()` and
  describe's `host_actions` are generated from them.
- Manifest keys: read `manifest.raw["<key>"]` (the whole openapp.json). Live channels for your output:
  `channels::publish(&k, "<module>/<name>", ChannelData::Json(v))`, `channels::subscribe(&k, topic, replay)`.
  Rate cap 120 msg/s per topic (excess coalesced, last-wins; `PublishOpts { concat: true }` appends bytes instead).
