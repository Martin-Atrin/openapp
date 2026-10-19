# OpenApp contract (v0.10)

Status: normative, current. Consolidated 2026-10-17 from `spec/MVP-CONTRACT.md` (v0.1 … v0.9.1, ten build waves)
against the code at `e7a9593` (+ guest image 0.9.1). **The code is the ground truth.** When this text and the code
disagree, fix this text. Planned work lives in [ROADMAP.md](ROADMAP.md); build history, spike numbers and wave notes
live in [IMPLEMENTATION-NOTES.md](IMPLEMENTATION-NOTES.md). The API tables in Part A §6 are generated from
`cargo run --example contract_dump` (§6.0). The kernel reports `contract: "0.10"` (`kernel::CONTRACT_VERSION`) in
`openapp_describe`.

---

# Part A — normative

## 1. Principles

1. **The kernel ships primitives, not features.** It ships an empty canvas, a chat composer, one agent connection
   (provider picker), the consent cards and construction tools. The agent builds every feature as a module in the
   user's workspace. Nothing restarts: tools act at once.
2. **The VM is the agent's world.** Release providers run inside a per-workspace Linux VM in bypass mode (no harness
   prompts). Everything that reaches the Mac or the internet crosses the kernel and is mediated there.
3. **Everything allowed, everything mediated.** Each capability has one consent class:

   | class | meaning | examples |
   |---|---|---|
   | **Free** | local, reversible, no data leaves the Mac; no card | layout, theme, spec/html views, channels, host services (on-device), app-wide shortcuts |
   | **Granted** | one kernel card, then remembered per target in the grant store; revocable | network host, microphone, camera, web control per site, LAN port, global shortcut, clipboard read, background, login item, host module source, folder |
   | **Confirmed** | a kernel card every time | `secret.release`, `secret.delete` |
   | **Invariant** | never allowed, by anyone (§14) | the agent answers its own card, edits kernel state, reads a secret value |

4. **Models are discovered, never hardcoded.** No model id literal in kernel, Pi templates or shell (enforced by a
   test). The kernel lists what the signed-in account offers at runtime (§7.4).
5. **Disk use stays bounded.** OpenApp must not grow without limit on the user's disk: one shared guest image
   (download refused without 2 GiB headroom), one sparse 20 GiB VM volume shared by all runtimes and caches (apt,
   npm), service logs capped at 5 MiB, per-module disk sampled and shown (`resources`). Generations, retention and a
   user-set budget are planned (ROADMAP §2).
6. **Honest gaps.** What the kernel cannot do is listed in `openapp_describe → kernel_gaps` with the closest
   workaround. The agent reports a gap in one sentence; it never builds UI that cannot work.

## 2. Processes and places

```
HOST (macOS) — OpenApp.app                                         APP VM (Linux arm64, one per workspace)
┌─────────────────────────────────────────────────┐                ┌───────────────────────────────────────┐
│ Shell (React, src/) — main webview + windows     │                │ harness: pi --mode rpc | codex app-   │
│   ⇅ Tauri IPC (§13), oamod:// oasvc://           │                │   server (bypass mode, own sign-in)   │
│ Kernel (Rust, src-tauri/src)                      │  bridge:       │ module MCP servers (exec stdio)       │
│  ├─ harness client: Pi RPC / Codex / ACP ────────┼─ msb exec ────►│ module services (setsid, supervised)  │
│  ├─ kernel HTTP 127.0.0.1:<rnd>: /mcp /host/*     │  stdio + vsock │ PTYs, Xvfb/noVNC displays             │
│  │   /channel (WS) /hook/<token>  ◄──────────────┼─ :7801 ────────┤ vsock relays (framed, acked)         │
│  ├─ egress proxy (HTTP CONNECT) ◄────────────────┼─ :7802 ────────┤ HTTP(S)_PROXY=http://<actor>@…:7802  │
│  ├─ egress SOCKS5 (same policy) ◄────────────────┼─ :7803 ────────┤ OPENAPP_SOCKS_URL=socks5h://…:7803   │
│  ├─ consent cards + grant store, research log    │                │                                       │
│  ├─ secrets (Keychain), net.fetch/imap, OAuth     │  virtio-fs     │ /workspace  ← host workspace (rw)     │
│  ├─ port gateway 127.0.0.1:<rnd>, LAN listeners   │◄══════════════►│ /opt/openapp ← persistent ext4 volume │
│  └─ host sidecars: openapp-macos (std library),   │                │ /mnt/<grant> ← granted folders        │
│     host modules (agent Swift), OpenAppNotify.app │                │ rootfs: fresh on every boot           │
└─────────────────────────────────────────────────┘                └───────────────────────────────────────┘
  ACP harnesses (claude, gemini, goose, opencode, custom) run on the host; gated off in release builds (§7.2).
```

- **Shell** = kernel chrome (composer, chat, picker, cards, presentations, build cards) + what modules place.
- **Kernel** = `src-tauri/src/kernel/` (core, no Tauri types; file map in `kernel/mod.rs`) + `primitives/`
  (registry, §6.0) + adapters (`pi.rs`, `codex.rs`, `acp.rs`) + `vm.rs`, `egress.rs`, `consent.rs`, `research.rs`.
- **App VM** = microsandbox 0.7.8 (libkrun, Hypervisor.framework); no network device (§8).
- **Bridge** = microsandbox exec sessions (piped stdio or PTY) plus three vsock forwards: guest `127.0.0.1:7801` → kernel
  HTTP server (MCP, `/host`, `/channel`, `/hook`), guest `127.0.0.1:7802` → egress proxy, guest `127.0.0.1:7803` → egress
  SOCKS5. Each route is a guest relay (`vm/vsock-relay.py`) and the host's `vm::deframe`: frames of at most 16 KiB,
  a host ack every 64 KiB (raw libkrun routes stall after 256 KiB of upload and lose writes above ~32 KiB). Login
  callback ports (1455, 53692) are forwarded host → guest only while a sign-in runs.
- **Host services / host modules** = Swift JSON-RPC sidecars on the Mac (§9).

## 3. Workspace and state dir

```
<workspace>/                     the user's app; visible to the agent; /workspace in the VM; git init on first run
  AGENTS.md, CLAUDE.md           builder guide, rewritten by the kernel on every start when it differs
  modules/<id>/openapp.json      module manifest (§4); modules/<id>/data/ = module data (recordings, inbox, downloads)
  .openapp/skills/openapp-builder/{SKILL.md, reference/*.md}   kernel-owned, rewritten on start when they differ
  .claude/skills/…, .claude/settings.json                     mirror for Claude Code (ACP); settings written if missing
  .openapp/logs/<id>.log, <id>.<service>.log, <id>.host.log (1 MB, rotated to .1), _agent.log, …    logs for debugging (agent-readable)

~/.openapp/state/<ws-id>/        kernel-owned, OUTSIDE the workspace, never mounted into the VM
  layout.json theme.json history.jsonl     layout/theme + undo history (changed only through tools)
  sessions/index.json sessions/<id>.json   chats (§13)
  research.jsonl                           research log (§12)
  grants.json folders.json                 grant store (§11), folder bookmarks
  harness.json models.json modules.json    provider choice, model catalogs/choices, applied default placements
  secrets.json secret_releases.json        secret metadata (values are in the Keychain), release records
  jobs.json jobs/<module>.<job>.jsonl      job schedule state, run logs
  shell.json safe_mode.json downloads-tmp/ menus/tray/shortcuts per module, safe mode, web-view downloads in flight
~/.openapp/vm/                   VM home (OPENAPP_VM_HOME; short path: unix sockets < 104 bytes): images/, runtime, run/
~/.openapp/host-build/<module>/<hash>/   approved host-module source snapshots + builds (OPENAPP_HOST_BUILD)
```

- `<ws-id>` = `ws-` + first 16 hex of SHA-256 of the canonical workspace path. `OPENAPP_STATE_HOME` moves the root,
  `OPENAPP_WORKSPACE` the workspace (default `~/.openapp/workspace`).
- **What the agent may see:** the workspace (and in the VM: `/opt/openapp`, `/mnt/<grant>`). It never sees the state
  dir, the Keychain, `host-build/`, other host paths, or `.openapp/` as an editing surface ("never edit `.openapp/`").
  Presentations, turn records and web views are in memory only.

## 4. Modules and the manifest

`modules/<id>/openapp.json`; `id` = folder name, `[a-z0-9-]+` (≤ 64). The kernel polls `modules/` every 500 ms: a
changed view file is re-read and pushed live; a changed manifest or server needs `openapp_module_load`. Status:
`stopped | idle | running | error`. Every key the code parses today:

| key | shape | parsed by |
|---|---|---|
| `id`, `name` | string (required) | modules.rs |
| `server` | `{ command: [..], env?: {}, net?: "socks" }` — MCP over stdio, cwd = module dir; in the VM when a VM harness runs | modules.rs |
| `views` | `{ <name>: { kind: "spec"\|"html", file } \| { kind: "web", url, profile? } }` | modules.rs |
| `placements` | `[{ view, anchor, label?, order? }]` — applied on the **first** load only | modules.rs |
| `host` | `{ swift: "<file>" \| package: "<dir>", product?, summary (required), service?, windows? }` | host_modules.rs, host_service.rs |
| `activation` | `["onStart"\|"onView"\|"onAction"\|"onJob"\|"onChannel"]` (default onView + onAction) | lifecycle.rs |
| `budget` | `{ cpu: %, rss_mb }` (default 90 % / 2048 MB; 3 samples over → toast + UI error) | lifecycle.rs |
| `channels` | `{ public: ["<name>" \| "<owner>/<name>" \| "prefix/*"] }` — what other modules may subscribe to | channels.rs |
| `services` | `[{ name, command, cwd?, env?, restart?: always\|on-failure\|never, health?: {tcp}\|{http}, ports?: [{port, expose?: view\|lan}], net?: "socks" }]` | services.rs |
| `apt` | `["pkg" \| "vendor/x.deb"]` — installed on every VM boot from the volume cache | services.rs |
| `arch` | `"amd64"` — services run under `qemu-x86_64` (explicit wrapper; no binfmt) | services.rs |
| `display` | `{ command, width?, height? }` — Xvfb + x11vnc + noVNC service `display` | display.rs |
| `jobs` | `[{ name, schedule, action? }]` (§6.2 jobs) | jobs.rs |
| `agents` | `[{ name, instructions, tools[], model?: default\|on-device, budget?: {turns, minutes}, triggers?[] }]` | agents.rs |
| `deeplinks` | `["prefix" \| { prefix, action }]` — `openapp://<module>/<path>`; default action tool `on_deeplink` | shell_misc.rs |
| `opens` | `[".eml" \| "audio/*" \| { types[], action }]` — "Open with OpenApp"; default tool `on_open` | shell_misc.rs |

Module-server and service env: `OPENAPP_MODULE`, `OPENAPP_HOST_URL`, `OPENAPP_HOST_TOKEN` (per module),
`OPENAPP_CHANNEL_URL`, `OPENAPP_SECRETS_DIR` (released files), `OPENAPP_AGENT_CLI` (Pi one-shot CLI when Pi is the
harness), `HTTP(S)_PROXY` and `OPENAPP_SOCKS_URL` (SOCKS5, opt-in) with actor `module.<id>`. `"net": "socks"` (VM only) runs the command
under proxychains4 (§8). Logs: stderr → `.openapp/logs/<id>.log` (never stdout for MCP).
Tool result directives: `structuredContent.openapp { composer_insert?, prompt?, notify? }` (prompt is skipped for
`openapp_call` test calls); after a tool action the kernel re-reads the module's views.

**Runtime rules.** *Lifecycle (G9):* at start an unactivated module loads `idle` (views and placements read, no
server); a visible view, a tool action or an outside subscriber starts it; no use for `OPENAPP_IDLE_SECS` (600) →
idle again (modules with a `restart: "always"` service or a host service never idle). CPU/RSS sampled every 5 s,
disk every 30 s → `resources`. Safe mode stops all modules and hides the layout (persisted).
*Jobs (G6):* `schedule` = cron (5 fields local time, 6–7 with seconds, `@daily`…), `{every: "1h30m"}`,
`{watch: "<glob>"}` or `{webhook: true}` (`http://127.0.0.1:<port>/hook/<token>`; `:7801` in the VM); `action` =
`{tool, args?}` | `{host, …}` | `{agent, input?}`; one run per job at a time; a missed run (sleep, app closed) runs
once on wake; logs in `<state>/jobs/`, events on `kernel/jobs`. *Agent modules (G10):* `agent.run` runs only the listed
tools (`<module>/<tool>`, `<module>/*`, `host:<method>`, `host:<module>/<method>`, `openapp:<tool|action>`) as
`module:<id>`; `model: "default"` = Pi SDK in the VM with the VM's sign-in (falls back to on-device when Pi is not
signed in), `"on-device"` = a guided-JSON loop on `ai.generate`; budget default 8 turns / 5 min; job `triggers` run it.
*Background (G6):* closing the window keeps OpenApp running while a job, an always-restart service, a host service
or a background grant exists (menu-bar item shows why).

## 5. Views

### 5.1 spec views (native, themed)
Node `{ type, ...props, children? }`. Catalog (`openapp_describe → spec_catalog` is complete):

| type | props |
|---|---|
| `stack` | `direction? column\|row`, `gap?`, `align? start\|center\|end\|stretch`, `justify? start\|center\|end\|between`, `padding?`, `children` |
| `text` | `value`, `variant? body\|title\|heading\|muted\|mono\|small` |
| `button` | `label`, `action`, `icon?`, `variant? primary\|secondary\|ghost\|danger` |
| `icon-button` | `icon` (lucide kebab-case), `label`, `action`, `active?` |
| `input` | `name`, `placeholder?`, `value?`, `submit?` (args get `{ value }`) |
| `toggle` | `label`, `value`, `action` (args get `{ value }`) |
| `list` | `items: [{ id, title, subtitle?, icon?, action?, active? }]`, `empty?` |
| `card` · `badge` · `progress` · `image` | `title?, children` · `label, tone? neutral\|accent\|success\|warning\|danger` · `value 0..1` · `src (http(s)/oamod://), alt?, height?` |
| `divider` · `slot` · `dropzone` | — · `id` (anchor `slot:<id>`) · `action` (gets `{ files }`), `label?, hint?, icon?, children?` |

- Unknown types render as a red error box and are reported in `openapp_snapshot`.
- **Action**: `{ tool, args? }` (own module server) or `{ host: "<host action>", ...args }` (§6.2).
- **Bindings**: any prop may be `{ "$host": "<source>" }` (host data, §6.4) or
  `{ "$stream": "<topic>", "path"?: "a.b", "default"? }` (live channel, re-renders on message).

### 5.2 html views (sandboxed iframe, per-module origin)
- URL `oamod://<module>/<file>`; files served from `modules/<module>/` (no `..`); the bridge script
  `oamod://<module>/_openapp/bridge.js` is injected first in `<head>`, plus zero-specificity base styles.
- iframe `sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-pointer-lock"`,
  `allow="camera; microphone; fullscreen; gamepad"`; transparent; height = content height.
- Microphone / camera: `openapp.requestMedia({audio?, video?})` → `MediaStream` after `media.request` (one card per
  module and kind; the bridge also runs it before every `navigator.mediaDevices.getUserMedia`). The WKWebView capture
  callback is wrapped (`primitives/media.rs`): in a shell webview the shell page itself is allowed, an
  `oamod://<module>` frame only with grant `microphone:<module>` / `camera:<module>` (no decision yet → denied, card
  shown, the page retries), any other frame denied; web surfaces keep WebKit's own prompt. No gate → capture denied.
- `window.openapp` (frozen): `call(tool, args)` · `host(action, args)` · `requestMedia(constraints) → MediaStream` · `subscribe(topic, cb) → unsubscribe` ·
  `publish(topic, data)` (own topics; ArrayBuffer → bytes) · `onDrop(cb) → unsubscribe` (files `[{path,name,size,mime}]`)
  · `dragOut({path})` · `onTheme(cb)`. CSS vars `--oa-<token>` are set on `<html>`.
- postMessage (objects with `oa`): iframe → shell `call | host | subscribe | unsubscribe | publish | resize | error |
  ready | key | dropzone`; shell → iframe `result | theme | channel | drop`. Accepted only from
  `event.source === iframe.contentWindow`. Keys are forwarded for app-wide shortcuts.

### 5.3 web views
`{ kind: "web", url, profile? }` → a Tauri child webview per mounted placement, persistent data store per `profile`
(default: module id; one Google login serves every view with that profile), desktop Safari UA. The shell reports the
rect with `web_bounds(placement, rect|null)`; child webviews draw above the shell, so nothing may overlap them and
cards hide them. Actions `web.navigate|back|forward|reload {placement}`; host data `web:<placement>` →
`{url, title, loading}`. Agent control and downloads: §10.4. Works in presentations (`present:<id>`) and windows.

### 5.4 Anchors, presentations, windows
- Anchors: `composer.actions` (buttons next to Send) · `composer.above` (strip above the input) · `main` (small pinned,
  collapsible strip above the chat) · `region:<id>` (`openapp_layout` region on any edge, `stack|tabs`) · `slot:<id>`.
- **Placement rules** (single source: the `<!-- placement-rules -->` block of SKILL.md → `describe.placement_rules`):
  always-on small controls → `composer.*` or a region; a tool opened sometimes → a button with
  `{host:"view.open", view, as:"sheet"}` (do not place the view); short form → `modal`; one-off answer → `inline`;
  side-panel tab → `as:"tab", region`; never a large view in `main`.
- Presentations (kernel chrome, memory only): `view.open { view, as: sheet|modal|inline|tab, title?, width?, region? }`
  → `presented[]` `{id: "pr_…", view, as, title?, width?, after?}`; views inside use placement id `present:<id>`;
  `view.close { view? }` closes the top one (and its guide strip).
- Windows: `window.open {...}` → own native window, placement id `window:<id>`; menu items use placement `menu`.
- Layout ops (`openapp_layout`, atomic): `region_add {id, edge, size, mode?, title?}`, `region_update`, `region_remove`
  (removes its placements), `place {module, view, anchor, label?, order?}` → `p_<8 hex>`, `move`, `unplace`.
- Theme (`openapp_theme`): colours `bg surface surface-2 text text-muted accent accent-text border success warning
  danger` (`#rgb #rrggbb rgb() hsl() oklch()`), `radius` (px), `font`, `font-mono`, `density compact|normal|comfy`.
  Unknown key or bad value → error; WCAG ≥ 4.5 for text/bg, text/surface, accent-text/accent.

## 6. Kernel API

### 6.0 How the API is built and dumped
Kernel built-ins (`primitives/chrome.rs`, `audio.rs`, `connect.rs`: `Builtin` tables) run first; then the
**primitive registry** (`primitives/<name>.rs`, one `Primitive` each: actions, MCP tools, `POST /host` endpoints,
host data, Tauri commands, slash commands, env, routes, event hooks, describe entry with class). A new primitive is one
line in `primitives::build()`. Dump everything:
`OPENAPP_STATE_HOME=<scratch> cargo run -q --example contract_dump -- <scratch-ws> <out.json>` →
`{ describe, mcp_tools (name, description, input_schema), host_actions (built-ins), kernel_methods }`.

### 6.1 MCP tools (server `openapp`, Streamable HTTP `/mcp`, bearer token; caller `agent`)

| tool | purpose | class |
|---|---|---|
| `openapp_describe` | contract, workspace, layout, theme, modules, anchors, catalog, host facts, primitives, gaps, … | Free |
| `openapp_module_load` / `_unload` | validate + (re)start a module / stop it (opt. remove placements); errors + 40 stderr lines | Free (host part: Granted card, waits ≤ 150 s) |
| `openapp_layout` · `openapp_theme` · `openapp_undo` | atomic layout ops · validated tokens · revert layout/theme history | Free |
| `openapp_call` · `openapp_snapshot` | call a module tool (test) · text tree of the screen + last 20 UI errors | Free |
| `openapp_host_action` | run chrome actions (`view.open/close`, `web.navigate/back/forward/reload`, `guide.set/clear`, `notify`, `open_url`, `composer.insert`), the agent-callable registry actions (`connect::AGENT_REGISTRY`: tray, menu, window, shortcut, dock badge, notify.post, clipboard.write, files.reveal, service, pty, job, agent, secret.release …) as `agent`, `model.request`, `safe.set {on: true}` | per action |
| `host_speech_transcribe` `host_ai_generate` `host_vision_ocr` `host_tts_speak` `host_tts_render` `host_translate` `host_nl_embed` `host_nl_language` `host_status` | macOS standard library (§9) | Free |
| `host_call` | a host-module method, or a std method when `module` is omitted | Free (module approved) |
| `openapp_secret_request` · `openapp_secrets` · `openapp_secret_delete` | secret card → Keychain · list metadata · delete | card · Free · Confirmed |
| `openapp_net_fetch` · `openapp_net_imap` | HTTP / read-only IMAP on the Mac with a secret injected | Granted per host |
| `openapp_oauth_authorize` · `openapp_oauth_device` | PKCE loopback in the system browser · device flow | user consents at provider |
| `openapp_web_read` `_click` `_fill` `_highlight` · `openapp_guide` | control a web view · guide strip | Granted per site · Free |
| `openapp_channel_publish` · `openapp_channel_read` | publish on a topic · read a topic's ring buffer | Free |
| `openapp_model_request` | ask for the strong model tier with a reason | policy `ask` (card) / `auto` |

### 6.2 Host actions (`{ "host": name, … }` from views, menus, tray, shortcuts, notifications, jobs; `openapp_host_action`)

| actions | purpose | class |
|---|---|---|
| `composer.insert` `prompt` `open_url` `notify` | composer text, message as the user, system browser, toast (`notify {title,…}` = `notify.post`) | Free |
| `session.new` `session.open` `session.rename` `mode.set` | kernel-owned sessions; chat/build mode | Free (user controls) |
| `harness.select` `harness.refresh` `harness.login` | provider picker, sign-in | Free (user controls) |
| `view.open` `view.close` `turn.undo` `guide.set` `guide.clear` `guide.done` | presentations, undo a build turn, guide strip | Free |
| `web.navigate` `web.back` `web.forward` `web.reload` | web view navigation | Free |
| `web.read` `web.click` `web.fill` `web.highlight` | agent/module control of a web view | Granted `web.control:<host>` |
| `audio.record` (`tool`, `arg?`, `transcribe?: "apple"`, `insert?`) | mic → `data/recordings/<ts>.m4a` → tool or on-device transcript | Granted `microphone:<module>` |
| `media.request {audio?, video?}` (`media.camera` = `{video: true}`) | lets the module's html views use `getUserMedia` (§5.2) | Granted `microphone:<module>` / `camera:<module>` |
| `host.call {method, params?, module?}` | std library / host module method | Free |
| `grant.revoke {id}` | remove a grant | Free (user) |
| `secret.request` `secret.delete` | secret card · delete | card · Confirmed |
| `net.fetch` `net.imap` `oauth.authorize` `oauth.device` | connect (§10) | Granted per host |
| `channel.publish` | views publish on their module's topics | Free |
| `service.status` `.restart` `.stop` `.logs` `.url` | VM services, `oasvc://` + gateway URLs | Free |
| `pty.open` `.resize` `.write` `.close` `.list` | terminals in the VM over channels | Free |
| `display.url` | noVNC page / VNC WebSocket of a module display | Free |
| `window.open` `.close` `.focus` `.fullscreen` · `menu.set` · `tray.set` `tray.clear` | native shell | Free |
| `shortcut.register` `shortcut.unregister` | app-wide shortcut / `global: true` | Free / Granted `shortcut.global` |
| `notify.post` · `dock.badge` · `clipboard.write` · `print` · `share` | notifications with buttons, badge, clipboard, print, share sheet | Free |
| `clipboard.read` | read text/image | Granted `clipboard.read:<module>` |
| `files.open` `files.save` `files.reveal` `files.quicklook` `files.drag_out` | native panels (user picks), Finder, Quick Look, drag out | Free (user action) |
| `files.grant_folder` `files.revoke_folder` | folder → `/mnt/<name>` in the VM (VM restarts ~1 s) | Granted (panel) |
| `job.create` `.update` `.delete` `.run_now` `.list` | jobs registry | Free |
| `background.request` · `app.login_item` | keep running with window closed · SMAppService login item | Granted |
| `secret.release` · `secret.revoke` · `secret.generate` | give a module the value · take it back · generate in Keychain (public part returned) | Confirmed · Free · Free |
| `agent.run` `agent.cancel` | run / cancel an agent module | Free (tools gated) |
| `model.set` `model.clear` `model.policy` `models.refresh` | model choice per slot, escalation policy, rediscovery | Free |
| `safe.set {on}` | safe mode: no modules, layout hidden | Free |

The agent's `openapp_host_action` refuses user-only actions (`connect::USER_ONLY`: sessions, providers, grants,
mode, `turn.undo`, `audio.record`, `prompt`, `host.call` — use `host_call` —, the panels `files.open/save/grant_folder`,
`print`, `share`, `files.drag_out`, model choices, `safe.set {on: false}`); the refusal says why and lists what the
agent may run. A unit test keeps every action classified. **Native shell state** (tray, menus, shortcuts, dock badge,
windows to open) is stored by the kernel; without an app window a call returns `{ applied: false, pending: true }`
and the shell applies it on attach; `openapp_describe → native_shell` and `openapp_snapshot` show it.

### 6.3 Host endpoints (`POST $OPENAPP_HOST_URL/<method>`, bearer `$OPENAPP_HOST_TOKEN`; caller `module:<id>`)
Errors: `HostError` code → HTTP status (`bad_request` 400, `forbidden` 403, `not_found` 404, `conflict` 409,
`upstream` 502, `internal` 500). The MCP token also works (caller `agent`).
- Std library: `speech.transcribe ai.generate ai.status vision.ocr tts.speak tts.render translate nl.embed
  nl.language status`; host modules: `/<module>/<method>`.
- Kernel methods (also MCP tools and `host_call`): `secret.request secret.delete secrets net.fetch net.imap
  oauth.authorize oauth.device web.read web.click web.fill web.highlight guide.set guide.clear`.
- Primitives: `channel.publish channel.read` · `service.status service.restart service.logs service.url` ·
  `pty.open pty.resize pty.write pty.close` · `display.url` · `window.open window.close window.focus window.fullscreen`
  · `menu.set` · `tray.set tray.clear` · `shortcut.register shortcut.unregister` · `notify.post notify` ·
  `dock.badge clipboard.write clipboard.read print share` · `files.open files.save files.grant_folder
  files.revoke_folder files.reveal files.quicklook` · `job.create job.update job.delete job.run_now job.list` ·
  `background.request app.login_item` · `secret.release secret.revoke secret.generate` · `agent.run agent.cancel
  agent.tool` (internal, per-run token). A module acts only on its own services, jobs, agents and topics.
- Other routes on the kernel HTTP server: `GET /channel` (WebSocket, `?token=`), `ANY /hook/<token>` (job webhooks).

### 6.4 Host data sources (`{ "$host": "<name>" }`; `snapshot.hostData`)
`sessions` · `session.active.title` · `agent.name` · `agent.status` · `harnesses` · `models` · `grants` · `secrets`
· `secret_releases` · `web:<placement>` · `channels` · `services` · `ptys` · `resources` · `safe_mode` · `jobs` ·
`agents` · `agent_runs` · `background` · `folders` · `windows` (`pending?`) · `menus` · `tray` · `shortcuts` · `dock` ·
`host_module_services` · `shell.icons` (shell internal). List sources return spec `list` items
(`{id, title, subtitle, icon, active?, action}`); shapes in `describe.host_data.sources`.

### 6.5 Channels (G1)
Topics `<owner>/<name>` (owner = module id, `host:<module>`, `kernel`). JSON or binary (base64 over IPC, raw over the
bridge), `seq`, `ts`. Ring buffer 64 per topic, subscriber queue 256 (drop-oldest), 120 msg/s per topic (excess
coalesced last-wins; PTY output concatenates), max 4 MiB per message. Publish only on own topics; subscribe to
another module's topics only if its manifest lists them in `channels.public`; `kernel/*` is readable by all.
Paths: views (`openapp.subscribe/publish`, `$stream`), module servers (WebSocket `$OPENAPP_CHANNEL_URL` ops
`subscribe|publish|unsubscribe` → `message`; binary frame = JSON header line + bytes; or `POST channel.publish`),
host modules (JSON-RPC `channel.publish`/`channel.subscribe`, kernel → `channel.message`).
**Kernel topics:** `kernel/sessions`, `kernel/agent`, `kernel/jobs` (`{op: start|end, job, run, status,
output_tail}`), `kernel/agents` (`{op: start|step|result|error|cancelled, run, agent}`), `kernel/notify`,
`kernel/vm-image` (guest image download progress), `kernel/deeplink` (every `openapp://` link).

### 6.6 Chat commands
`/build [text]` · `/chat` · `/undo [n]` · `/reset` (empty canvas + default theme; modules kept) · `/provider`
(back to the picker) · `/account` (the account card, §7.3) · `/model [chat|build|strong] <name> [effort] | policy ask|auto | refresh | default` ·
`/safe` / `/safe off`. Handled by the kernel, never sent to the agent.

## 7. Providers

### 7.1 Picker
First screen when no provider is picked (agent status `unselected`). `HarnessInfo = { id, name, adapter:
"pi-rpc"|"codex-app-server"|"acp", available, signedIn: bool|null, note?, vm?, login?: { state:
required|starting|browser|device|failed, url?, code?, message? }, accounts?: { logins: [{ provider, type, label,
active }], methods }, comingSoon? }`. Choice persists in
`<state>/harness.json`; `harness.select` stops the old harness and starts the new one; sessions stay; per-harness
thread/session mappings. `OPENAPP_HARNESS` (tests) overrides the saved choice.

| id | name | adapter | where |
|---|---|---|---|
| `pi` | Pi · isolated VM | Pi RPC (`pi --mode rpc`) | VM |
| `codex` | Codex · isolated VM | Codex app-server | VM (debug-build tests: host via `OPENAPP_CODEX_CMD`) |
| `claude` `gemini` `goose` `opencode` | Claude Code, Gemini CLI, Goose, OpenCode | ACP | host |
| `custom` | from `OPENAPP_AGENT_CMD` | ACP | host |

### 7.2 Release gating
`harness::RELEASE_PROVIDERS = ["pi"]`. Gating is on in release builds and in debug with `OPENAPP_RELEASE_GATING=1`:
only `pi` can be probed, selected, loaded or started; `OPENAPP_AGENT_CMD`/`OPENAPP_HARNESS` for other ids are
refused. The others stay in `harnesses[]` as `{available: false, signedIn: null, note: "Coming soon", comingSoon:
true}` without any PATH or login probe; the picker shows them dimmed and inert.

### 7.3 Pi (the release path)
- `@earendil-works/pi-coding-agent` **1.3.0** (MIT), npm-installed through the proxy (actor `kernel`) into
  `/opt/openapp/runtimes/pi/1.3.0`; `PI_CODING_AGENT_DIR=/opt/openapp/home/pi`; `PI_OFFLINE=1`,
  `PI_SKIP_VERSION_CHECK=1`, `PI_TELEMETRY=0`.
- Kernel-owned agent dir: `SYSTEM.md` (OpenApp's whole system prompt: containment, VM, mode and build rules, tool
  list, MCP instructions), `mcp.json` (server `openapp` at the guest forwarder, `Bearer ${OPENAPP_MCP_TOKEN}`, timeout
  600 s), merged `settings.json` (skills `/workspace/.openapp/skills`, no project trust, no telemetry),
  `extensions/openapp.ts` (`ask_user`, `/openapp-check`, `/openapp-models`), `openapp-login.ts`.
- One process, one Pi session at a time: turns serialized; OpenApp session ↔ Pi session file (`new_session` /
  `switch_session`). Build turns send `/skill:openapp-builder` + build prefix + text. `text_delta`/`thinking_delta`
  → agent/thought items; `tool_execution_*` → tool chips (bash: command, output tail 4000, exitCode); end =
  `agent_settled`; cancel = `abort`. Extension UI: `ask_user` → question card; `select/input/editor` → one-question
  card; `confirm` → permission card.
- Sign-in: ChatGPT or Claude OAuth inside the VM (callback 1455 / 53692 forwarded; or paste the redirect URL with
  `harness_login_submit(id, "code", value)`), or an API key (`harness_login_submit(id, "apikey", key, provider)` →
  VM `auth.json`, 0600, never logged). After sign-in Pi restarts; sign-in writes no model.
- Account card (kernel chrome: model chip › Accounts…, OpenApp menu › Accounts…, `/account`): the stored logins
  (`HarnessInfo.accounts`, never a value) with Sign out (`harness_logout(id, provider)` removes it from `auth.json`;
  another login becomes the default; Pi restarts), sign in again, another provider, or a new API key (replaces the
  provider's key). Pi refreshes OAuth tokens itself; when a turn fails because the provider refused the login (401,
  expired or revoked token, wrong key), the harness turns `signedIn: false` and the sign-in card opens with the
  reason.

### 7.4 Codex and ACP (gated)
- Codex `0.162.1` in `/opt/openapp/runtimes/codex/0.162.1`, `CODEX_HOME=/opt/openapp/home/codex`, approval `never`,
  sandbox `danger-full-access`; sign-in through app-server `account/login/start` (`chatgpt` | `chatgptDeviceCode`);
  build turns add a `skill` input item; `skills/extraRoots/set`; `developerInstructions` carry the containment rules.
- ACP: `initialize` (protocol 1, no fs/terminal), `session/new|load` with the `openapp` HTTP MCP server, `session/prompt`,
  `session/update` mapping, `session/request_permission` → permission card, `session/cancel`; unknown requests → -32601.
  Build turns prepend "follow the skill at <path>".

### 7.5 Models (§23 behaviour)
- Discovery after start/sign-in, on `models.refresh`, every 6 hours while Pi runs, daily: Codex `model/list` (paged, `includeHidden`); Pi
  `/openapp-models` (Pi's catalog filtered by the account's live list fetched with Pi's own credentials); ACP
  `configOptions` (category `model`, `thought_level`) or `models.availableModels`.
- Catalog `{id, displayName, description, efforts[], defaultEffort, hidden, isDefault, harness}` in snapshot
  `models`, describe and host data; additions/removals research-logged; a model the account newly offers is named
  in one system line ("New from your account: …").
- Choice per harness: slots `chat`, `build` (falls back to chat), `strong` of `{model, effort}` in `models.json`.
  No choice → catalog `isDefault`, else first non-hidden — never a literal. Applied per turn (Codex `model`/`effort`;
  Pi `set_model` + `set_thinking_level`; ACP `session/set_config_option`/`session/set_model`).
- Efforts: one ladder `none|off < minimal < low < medium < high < xhigh < max < ultra`, nearest supported step.
- Escalation: `openapp_model_request {tier: "strong", reason}` → policy `ask` (kernel card) or `auto`; Pi switches
  mid-turn, Codex/ACP finish and the kernel runs one follow-up turn on the strong slot; then it reverts.
- A model that leaves the catalog, or that the provider rejects as unavailable, is dropped (`rejected`) with one
  system line, and the default is used.
- Snapshot: `models[]`, `modelPrefs`, `agent.{model, effort, modelName, tier}`; shell model chip next to the mode chip.

## 8. VM and network

- **Runtime:** microsandbox 0.7.8 + libkrunfw 5, bundled in the .app (`bundle.rs`: `Contents/MacOS/msb`,
  `Contents/Frameworks/libkrunfw.5.dylib`; a release build missing a piece stops at startup). 2 vCPU / 4096 MiB
  default (`OPENAPP_VM_CPUS`, `OPENAPP_VM_MEMORY_MIB`). One VM per workspace; boots at start when the selected
  harness runs in the VM (before modules autoload), else on first use (services, PTY, folder grant); stops on quit.
- **Guest image `0.9.2`** (`vm::GUEST_IMAGE_VERSION`): Debian trixie-slim arm64, node 24 in `/usr/local`, python3,
  git, ffmpeg, build-essential, curl, ripgrep, fd, socat, bubblewrap, Xvfb + x11vnc + noVNC, qemu-user (x86_64,
  i386), proxychains4 (`/etc/proxychains4.conf` → `127.0.0.1:7803`, `proxy_dns`, `localnet 127.0.0.0/8`). Archive `<vm home>/images/openapp-guest-<ver>.tar`, else downloaded once from `GUEST_IMAGE_URL` (zstd,
  sha256 + size checked, resume/retry, progress on `kernel/vm-image`; needs 2 GiB free beyond the download).
  `OPENAPP_GUEST_IMAGE_TAR` / `OPENAPP_GUEST_IMAGE_URL` override. Built with `node src-tauri/vm/guest-image/build.ts`.
- **Mounts:** `/workspace` (virtio-fs, stat virtualization off; host edits can take ~5 s to appear — kernel files the
  guest needs at once are written through the bridge), `/opt/openapp` (ext4 volume `openapp-opt`, 20 GiB sparse:
  runtimes, homes and logins, apt cache), `/mnt/<grant>` (folders). The rootfs is fresh on every boot.
- **Services (G2):** each is one exec session under `setsid -w` with a pidfile (`/run/openapp/svc`); TERM then KILL
  after 3 s; restart backoff 1 s … 30 s (reset after 30 s up); health every 2 s, 5 failures after being healthy →
  restart; logs `.openapp/logs/<module>.<name>.log` (5 MiB cap). Start on module load (unchanged specs keep
  running across reloads), stop on unload. Daemons needed before any view: `activation: ["onStart"]` +
  `restart: "always"`.
- **apt per module:** packages (and module `.deb` files) installed on every boot before services start, from the
  cache on the volume (no network after the first install), through the proxy (actor `apt`).
- **amd64:** `"arch": "amd64"` wraps service commands with `qemu-x86_64` (binfmt_misc is unavailable). Slow.
- **Ports:** only ports a loaded module declares (`services[].ports`, `display`) are reachable.
  `oasvc://<module>/<port>/<path>` proxies HTTP per request (redirects and absolute paths rewritten). WebSockets use
  the **gateway** `http://127.0.0.1:<g>/<per-module token>/<module>/<port>/` (raw pass-through after the request
  line). `service.url` returns both. `expose: "lan"` → Granted card (`lan:<module>:<port>`), host listener on
  `0.0.0.0:<port>`, Bonjour `_http._tcp` via `dns-sd`.
- **PTY:** real pseudo-terminal (exec `tty: true`), default `bash -l` in `/workspace`, `TERM=xterm-256color`; output
  topic `<module>/pty/<id>` (binary, then `{type:"exit", code}`), input `<module>/pty/<id>/in` (bytes, text or
  `{resize}`); closed on unload.
- **Egress:** the VM has no network device. Guest processes get `HTTP(S)_PROXY=http://<actor>@127.0.0.1:7802`,
  `NO_PROXY=localhost,127.0.0.1,::1`, `NODE_USE_ENV_PROXY=1`; actor = `agent`, `module.<id>`, `kernel`, `apt`, else
  `guest` (self-reported). Default allow (`egress::DEFAULT_ALLOW`; `.x` = subdomains): `api.openai.com`,
  `auth.openai.com`, `chatgpt.com`, `.chatgpt.com`, `api.anthropic.com`, `platform.claude.com`,
  `registry.npmjs.org`, `pypi.org`, `files.pythonhosted.org`, `huggingface.co`, `.huggingface.co`, `.hf.co`,
  `deb.debian.org`, `security.debian.org`, `github.com`, `objects.githubusercontent.com`,
  `release-assets.githubusercontent.com`. Else a grant `network:<host>`, else a card **Allow once** (host admitted
  2 min) · **Allow for this workspace** (grant) · **Deny** (refused 1 min); concurrent connections share one card;
  card timeout 10 min. Loopback, link-local and unspecified targets are always refused. Every connection is logged
  (`egress`: host, port, bytes, decision, actor). A denied host returns 403.
- **SOCKS5 egress** (non-HTTP protocols: IMAP, MTProto, MQTT, raw TCP): RFC 1928 `CONNECT` at guest `127.0.0.1:7803`
  (vsock → `run/<vm>/socks.sock` → `egress::serve_socks`); methods no-auth (actor `guest`) and RFC 1929 user/password
  (user = actor, password ignored); names are resolved on the Mac (socks5h); `BIND` / `UDP ASSOCIATE` → reply 7. Same
  `decide` (default list, grants, cards, timers), local-address refusal and `egress` log (`method: "SOCKS5"`,
  `ip_literal`). Card text: "<actor> wants to reach imap.gmail.com:993"; an IP literal: "<actor> wants to reach the IP
  address 149.154.167.50:443" (port always shown). A denied target gets reply 2. SOCKS is opt-in: guest processes
  get `OPENAPP_SOCKS_URL=socks5h://<actor>:openapp@127.0.0.1:7803` to pass as a library's proxy option; `ALL_PROXY` is
  never set (it would move every httpx / curl client to SOCKS, and httpx without `httpx[socks]` refuses to start).
  `"net": "socks"` on a service or server (zero config) wraps its command with
  `proxychains4 -q -f /tmp/openapp-proxychains-<actor>.conf` (`vm::socks_wrap`; not under qemu-user).
- **Sockets and quit:** `run/<vm>/{mcp,proxy,socks}.sock` + `owner.pid` (kernel pid). Quit closes the sockets first,
  synchronously, then stops the VM (≤ 8 s). `vm::running_vms(home)` reports a VM only when its owner is alive and the
  socket accepts, and removes stale sockets (the probes / bake-off check); `AppVm::start` refuses a run dir owned
  by another live process.

## 9. Host services and host modules

- **Standard library** (`openapp-macos`, Swift 6 / macOS 26 SDK, `src-tauri/macos-services/`, built by `build.rs`;
  `OPENAPP_SKIP_SWIFT=1` skips; bundled `Contents/MacOS/openapp-macos`): `speech.transcribe {path, locale?}`,
  `ai.generate {prompt, instructions?, schema?, temperature?}` → `{text}|{json}`, `ai.status`, `vision.ocr {path,
  languages?}`, `tts.speak`, `tts.render {text, path}`, `translate {text, from?, to}` (installed pairs only),
  `nl.embed {texts[]}` (512 dims), `nl.language`, `status`. Free class; research `host_service` (method, ms, sizes,
  never content). Supervised (≤ 5 starts/min), per-method timeouts.
- **Paths** for host methods: `/workspace/...`, workspace-relative, absolute inside the workspace, or `/mnt/<grant>/...`
  (`paths.rs`); everything else is refused. Outputs are written inside the workspace.
- **Callers:** agent (`host_*`, `host_call`), module servers (`POST $OPENAPP_HOST_URL/<method>`), views (`host.call`),
  `audio.record` with `transcribe: "apple"` (`insert: true` → composer, or `tool` gets `{text, path, locale}`).
  Recordings are AAC `.m4a` (`audio/mp4`); webm is refused for transcription.
- **Host modules** (`"host": { swift | package, product?, summary, service?, windows? }`): agent-written Swift the
  kernel compiles on the Mac. Consent per **source hash** (SHA-256 of relative paths + contents): a permission card
  with summary, imported frameworks, shell/AppleScript calls, files, diff vs the last approved source, "opens its own
  windows" when relevant. Approval record + approved snapshot live in `~/.openapp/host-build/<module>/<hash>/`; only
  that snapshot is compiled (`swiftc` / SwiftPM); `grants.json host_modules.<id>` mirrors the decision. Every host
  binary embeds an Info.plist (`__TEXT,__info_plist`, linker `-sectcreate`): `CFBundleIdentifier
  com.polygonface.openapp.host.<module>` and the usage descriptions of `host_modules::USAGE_DESCRIPTIONS` (the same
  list as `src-tauri/Info.plist`, test-enforced: Bluetooth, Contacts, Calendars, Reminders, Photos, Camera,
  Microphone, Location, AppleEvents, Speech, screen capture, Desktop/Documents/Downloads, Apple Music, local network
  + `NSBonjourServices`). Outside OpenApp.app (dev, tests, probes) a host module is spawned responsible for itself
  (`responsibility_spawnattrs_setdisclaim`), so TCC reads its own plist instead of killing it (SIGABRT). A host module
  speaks the same JSON-RPC, may answer `methods` `[{name, description}]`, and is callable as
  `host_call {module, method}` / `POST /host/<module>/<method>`. Its stderr goes to
  `<workspace>/.openapp/logs/<module>.host.log` (rotated to `.1` at 1 MB; `host_part.log` in the load result). A call
  times out after 60 s; when the module imports a privacy (TCC) framework (`host_modules::TCC_FRAMEWORKS`) the error
  says "the host module did not answer in 60 s — macOS may be showing a permission prompt to the user".
- **Long-running host modules** (`service: true`): start with the module, never idle-stop, restart with backoff
  1 s … 30 s (research `host_service_exit`), may publish/subscribe channels and open AppKit windows through the
  `OpenAppHost` runtime (`OpenAppHost.method/publish/subscribe/openWindow/closeWindow/run`; built-in
  `openapp.windows`). Host data `host_module_services`.

## 10. Secrets, OAuth, net.fetch, web control

1. **Secrets** (`secrets.rs`): Keychain generic passwords, service `com.polygonface.openapp/<ws-id>/<name>`
   (`OPENAPP_KEYCHAIN_PREFIX` for tests), account `<name>`; metadata in `<state>/secrets.json`. Kinds `token`,
   `password`, `oauth-client`, `oauth-token`, `keypair`; requestable: `token`, `password`, `oauth-client`.
   `secret.request {name, label, kind, help?, url?, hosts?}` → secret card (`kind: "secret"` chat item, masked
   input); `submit_secret(request_id, value|null)`; a bad value keeps the card open; a tool call waits ≤ 9 min then
   returns `pending` (card stays 30 min; same name joins it). The agent gets `{name, ok}` only. Values never enter
   chat, research, logs, the VM, MCP results or HTTP replies — except an explicit `secret.release`.
2. **net.fetch** `{url, method?, headers?, body?|json?|body_base64?, auth?: {secret, scheme?: bearer|basic|
   header:<name>|query:<name>, username?}}` runs on the Mac (reqwest), applies egress per actor, refuses local
   addresses (DNS answers included), follows ≤ 5 redirects (egress check per hop, credential only on the original
   host), scrubs the credential from replies (`[secret]`), body ≤ 10 MiB. **Host binding:** a secret with `hosts`
   (or a preset OAuth token) goes only to those hosts; others ask once per new host and extend `hosts`.
   **net.imap** `{host, secret, username, op: list|search|fetch, …}`: read-only, TLS 993, LOGIN or XOAUTH2.
   SMTP is not built (send through provider APIs with `net.fetch`).
3. **OAuth** (`oauth.rs`): `oauth.authorize {client, scopes[], provider?, token?, extra?}` = authorization code + PKCE
   S256, loopback `http://127.0.0.1:<random>/callback` in the system browser, state checked, 5 min; token stored as
   `oauth-token` `{access_token, refresh_token, expires_at, scopes, client, token_url, provider}`, refreshed when
   < 60 s left or after a 401 (one retry). `oauth.device` for providers with a device flow. Presets (endpoints only,
   no client ids): google, github, microsoft, slack, notion, dropbox, spotify. Preset token endpoints skip the egress
   card; custom endpoints do not.
4. **Web control + guided consoles** (`web_control.rs`): `web.read` (URL, title, ~8k chars of text, elements with
   stable refs in `data-oa-ref`), `web.click`, `web.fill`, `web.highlight {note?, clear?}`; grant `web.control:<host>`
   per site; no raw eval. `web.click` refuses elements matching
   accept/agree/allow/authorize/grant/consent/pay/purchase/buy/subscribe/billing/upgrade/delete/publish/create/enable/
   confirm — the agent highlights them and the user clicks. Guide strip `guide.set {placement?, steps, current}`; Done
   → user prompt "Done: <title>" and the next step. Downloads land in `<state>/downloads-tmp/`; an OAuth client JSON
   → a card → secret `<provider>-client`; other files → `modules/<id>/data/downloads/`.
5. **Secret release + generate** (G7): `secret.release {name, module, as: "env:NAME" | "file:<path>"}` → Confirmed
   card every time; env at every start of the module's server and services; file 0600 at
   `/run/openapp/secrets/<module>/<path>` (guest tmpfs, never on host disk or in the workspace); the module restarts.
   `secret.revoke` removes it. `secret.generate {name, kind: password|ed25519|x25519|rsa-4096}` stores the private
   part and returns the public part only.

## 11. Consent and grants

- **One card path** (`consent.rs`): `open/close` (agent permission requests from Pi/Codex/ACP: the answer goes to the
  agent; cancelled when the turn ends), `ask` (kernel-owned card, research-logged, optional timeout, not cancelled by
  turn end), `granted` (stored decision, else ask, then store), `module_grant` (Granted per module). Chat item
  `kind: "permission"` with `options [{optionId, name, kind}]`, optional `diff`, `details`; answered only through
  `respond_permission` from the shell.
- **One store** (`kernel/grants.rs`): `<state>/grants.json` = `{ "<kind>": { "<target>": { allowed, scope:
  "workspace", ts, ...extra (actor, source_hash) } } }`; bare booleans from old files are read and rewritten; atomic
  writes (temp + rename) under one lock. Kinds: `network` (host), `web.control` (host), `lan` (`<module>:<port>`),
  `microphone`, `camera`, `clipboard.read`, `shortcut.global`, `background`, `login_item`, `host_modules` (module).
  Folder grants are bookmarks in `<state>/folders.json`.
- Snapshot / host data `grants`: `[{id: "<kind>:<target>", kind, target, allowed, scope, ts}]` (+ folders);
  `grant.revoke {id}` (LAN revocation applies on next start).
- Neither the agent nor a module can write the store, answer a card, or grant itself or another module anything.

## 12. Research log

`<state>/research.jsonl`, append-only, one `{ ts, actor, kind, data }` per line; the agent cannot read or edit it.
**Actor rule:** the actor is who caused the event — `user`, `agent`, `kernel` (on its own), `module:<id>`,
`view:<id>`, `host:<id>`, `agent:<id>`. When someone else asked first, the actor is whose action made it happen and
`data.by` names the requester. **Privacy:** never tokens, secret values, env values, host-service content, job output
or agent-module content (tool names and status only).
Kinds (`research::Kind`, one list): `prompt session_new session_open mode agent_message tool_call turn_end turn_model
question permission undo` · `harness_select harness_login harness_account harness_config harness_install` ·
`models_discovered model_choice model_policy model_request model_switch` · `layout theme present ui_action ui_error
host_action shell_action` · `module_load module_error activate idle_stop budget safe_mode` · `vm_start vm_error
vm_image vm_remount egress` · `audio_record host_service host_module_build host_service_start host_service_exit
grant_revoke` · `secret net_fetch net_imap oauth web_control guide download` · `secret_release services_start
services_stop services_error service_exit service_restart service_stop apt_install lan_expose lan_connection pty_open
window menu tray shortcut notify notify_response clipboard share drop deeplink open_file files_open files_save
folder_grant folder_revoke job job_run jobs_wake login_item agent_run agent_tool`.

## 13. IPC (shell ⇄ kernel)

**Commands:** `get_snapshot` · `send_prompt(text)` · `cancel_turn` · `respond_permission(request_id, option_id|null)`
· `ui_action(placement_id, action, input?) → result` · `iframe_call(module, tool, args)` · `web_bounds(placement_id,
rect|null)` · `answer_question(request_id, answers{id: string[]})` · `save_audio(placement_id, data, mime, action?,
duration_ms?)` · `select_harness(id)` · `harness_login(id, method?)` · `harness_login_submit(id, kind, value,
provider?)` · `harness_logout(id, provider)` · `submit_secret(request_id, value|null)` · `channel_subscribe(placement_id, topic)` ·
`channel_unsubscribe(sub)` · `channel_publish(placement_id, message)` · `primitive_command(name, args?,
placement_id?)` (`shortcut.fire`, `files.drop`, `shell.icon`) · `report_ui_error(placement_id, message)`.
**Events:** `oa://snapshot` (Snapshot) · `oa://chat` (`{op: append, item} | {op: update, id, patch}`) ·
`oa://composer` (`{insert}`) · `oa://notify` (`{text}`) · `oa://channel` (subscribed messages) · `oa://shell`
(window-mode helpers). **Protocols:** `oamod://` (module files), `oasvc://` (VM ports). Deep links `openapp://` and
"Open with" arrive through Tauri `RunEvent::Opened` (bundled builds only).
**Snapshot** = `{ layout, theme, modules[{id, name, status, error?}], views{"<m>/<v>": spec|html|web|error}, agent{name,
status: unselected|starting|ready|working|error, harness, model…}, chat[], sessions[{id, title, created, updated,
mode}], activeSession, activeMode, harnesses[], web{}, grants[], presented[], turns[{id, changes, modules, undone,
undoable}], secrets[], guide?, hostData{}, models[], modelPrefs }`.
**Chat items** `kind: user|agent|thought|tool|permission|question|secret|system|error`; items carry `turnId` + `mode`.
Build turns render as one build card (title, live status line, cards inline, final summary, **Undo** = `turn.undo`,
**Show details** = the hidden tool/command log). Sessions `s_<8 hex>`, title = first prompt (≤ 48 chars).
Questions `{questions: [{id, header, question, options|null, isOther, isSecret}], resolved}`.

## 14. Kernel invariants

Enforced today:
1. The agent cannot change the kernel, its chrome, the contracts or the research log: kernel state is outside the
   workspace and never mounted into the VM; kernel-owned workspace files (`AGENTS.md`, skills) are rewritten on start.
2. A capability comes only from a human action on a kernel card or panel. No agent or module answers a card, writes
   `grants.json`, or approves its own host-module source (approval lives outside the workspace, keyed by source hash).
3. Secret values never enter agent context, chat, the VM, logs or the research log; use by reference (`net.fetch`,
   `net.imap`, OAuth). The one exception is a user-Confirmed `secret.release` to one module.
4. The VM reaches the network only through the egress proxy, and never the Mac's loopback / link-local addresses;
   `net.fetch` refuses local addresses too.
5. Recovery is kernel-owned and works with a broken agent or modules: `/undo`, `turn.undo`, `/reset`, `/safe` + the
   kernel menu item, `/provider`.
6. The research log is append-only and kernel-owned; every entry names its actor (§12).
7. A theme that fails validation or contrast never goes live.
8. Models are never hardcoded (test-enforced).
9. Release builds run only `RELEASE_PROVIDERS`; gated providers are never probed.

Not yet enforced (tracked in ROADMAP §6): per-module file isolation inside the VM (modules share `/workspace` and the
guest's loopback), no confused deputy, untrusted-content labelling, research export/purge controls, design-system
components beyond theme tokens, host-side ACP harnesses outside any sandbox (debug only).

---

# Appendix — Changes from MVP-CONTRACT (code wins)

Old section numbers (≈318 `§N` citations remain in code comments) map as: §1 → 2 · §2 → 3 · §3 → 4 · §4, §13, §19.1
→ 5 · §5, §10, §21 → 6 · §6 → 5.4 · §7 → 12 · §8 → 13 · §9, §11, §12, §17.2, §23 → 7 · §15 → 6.2/9 · §16, §17,
G2/G3 → 8 · §18, §18.1, G8 → 9 · §20, G7 → 10 · §14, §22 → ROADMAP.

- Version label 0.10 = `kernel::CONTRACT_VERSION` (was "0.3" until loop 2).
- Release path is **Pi**, not Codex: `RELEASE_PROVIDERS = ["pi"]`; Codex and ACP are gated "Coming soon".
- `AGENTS.md` / `CLAUDE.md` are rewritten on every start when they differ (old text: "if missing").
- The pre-v0.7 `.openapp/` → state-dir migration is gone (removed as dead code). `state.rs` header still says
  `<workspace>/.openapp/`; the state dir is correct.
- ACP `elicitation/create` → question card is not implemented (old §12.4 said "where possible").
- html iframe sandbox gained `allow-pointer-lock`; `allow` delegates camera + microphone to every view and the kernel's
  capture gate enforces the per-module grant (was: `camera` in `allow` only after the grant, no microphone).
- Spec catalog gained `dropzone`; bindings gained `$stream`; `window.openapp` gained `subscribe`, `publish`, `onDrop`,
  `dragOut`.
- Module status gained `idle` (lazy activation); manifest keys are listed in one table (§4), incl. `deeplinks`
  `{prefix, action}` / `opens` `{types, action}` object forms.
- Egress default list gained `.hf.co`, `platform.claude.com`, Debian and GitHub release hosts; `apt` is an actor.
- Loop 3: SOCKS5 egress (`:7803`, `OPENAPP_SOCKS_URL`, `"net": "socks"`, guest image 0.9.2 with proxychains4); agent
  `openapp_host_action` runs an explicit registry allowlist (was: every registry action) and refuses user-only
  actions with the allowed list; native shell state is stored and applied on attach (`pending`); stale VM sockets.
- Kernel topics gained `kernel/vm-image` and `kernel/deeplink`; channel max message 4 MiB, subscriber queue 256.
- The guest image is downloaded (sha256-checked) when no local archive exists; `GUEST_IMAGE_REPO` is still a
  placeholder (ROADMAP §4).
- Consent is one path (`consent.rs`) and one grant store with one shape; `grants.json` kinds listed in §11.
- `openapp_call` skips the `prompt` directive; `openapp_host_action` runs only the listed chrome actions plus
  registry actions.
- Planned sections moved to ROADMAP.md: intelligent thread (§14 incl. `openapp_show`, charts, MCP App widgets),
  OS generations / disk budget / system journal / starter packs (§22), smtp, packaging, signing, CSP.
