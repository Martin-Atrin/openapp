# Services, ports, terminals, displays, activation

## Services and daemons (long-running programs in the VM)
Declare them in `openapp.json`; the kernel starts them after the module loads, restarts them (backoff 1–30 s),
health-checks them, stops them on unload and keeps them running across reloads when the declaration is unchanged.
```json
{ "id": "mail", "name": "Mail", "activation": ["onStart"],
  "apt": ["jq", "vendor/tool_1.2_arm64.deb"],
  "services": [{ "name": "imap", "command": ["sh", "-c", "exec ./bin/daemon --port 1143"],
                 "env": { "LOG": "info" }, "restart": "always", "health": { "tcp": 1143 } }] }
```
- `command` is an array (`["sh","-c","…"]` for a shell line); cwd = the module folder (or `"cwd"`); env = `HTTP(S)_PROXY`
  and `OPENAPP_SOCKS_URL` (SOCKS5 for non-HTTP, pass it as a proxy option; `"net": "socks"` runs a program without proxy support under proxychains4; actor `module.<id>`), `OPENAPP_HOST_URL/TOKEN`, `OPENAPP_CHANNEL_URL`, plus yours. Stdout/stderr →
  `.openapp/logs/<id>.<name>.log`. `restart`: `always` | `on-failure` (default) | `never`. `health`: `{ "tcp": port }`
  or `{ "http": "http://127.0.0.1:8080/health" }`; a service that was healthy and then fails 5 checks (10 s) is restarted.
- The VM's root filesystem is fresh after every VM start; `/opt/openapp` and `/workspace` persist. Packages go into
  `"apt"` (Debian packages or `.deb` files in the module folder), installed before the services start through the
  proxy and cached, so later starts reinstall offline. Not a one-off `apt-get`.
- `"arch": "amd64"`: x86_64 binaries run under qemu-user (slow, ~5–20×; static binaries work as is, dynamic ones need
  `dpkg --add-architecture amd64` + the `:amd64` libs). Prefer arm64 builds or building from source.
- **Every long-running daemon is a declared service** with `"activation": ["onStart"]` and `"restart": "always"`, so it
  runs (and logs in, syncs, listens) before any view opens. Never spawn it from a view, from a tool call or with a
  one-off shell command: it would stop with the VM and the user would find it dead. The UI only reads its state.
- Status: `openapp_describe → services`, host data `{ "$host": "services" }`, host actions `service.status {module?}`,
  `service.restart {module, name}`, `service.stop {module, name}`, `service.logs {module, name, lines?}` (servers:
  `POST $OPENAPP_HOST_URL/service.status` etc., their own module only).
- Your module server talks to its daemon on `127.0.0.1:<port>` inside the VM (retry until the health check passes).

Sketch — **Proton Mail** through Proton Mail Bridge (GPL-3; IMAP 127.0.0.1:1143, SMTP 1025 inside the VM):
1. Proton's Linux packages are x86_64 only. Best: build it from source once: `"apt": ["golang", "libsecret-1-dev",
   "pass", "gnupg", "make"]`, `git clone https://github.com/ProtonMail/proton-bridge`, `make build-nogui`, keep the
   binary in `modules/mail/vendor/`. Fallback: the official `.deb` from proton.me in `"apt"` with `"arch": "amd64"`.
2. Bridge needs a keychain: `pass` with a GPG key made once (`GNUPGHOME`, `PASSWORD_STORE_DIR` under `data/`).
3. Service: `{ "name": "bridge", "command": ["sh", "-c", "exec ./vendor/bridge --noninteractive"], "restart": "always",
   "health": { "tcp": 1143 } }`.
4. Login never in chat: `secret.request` the Proton password and `secret.release` it to the module as an env var
   (see `jobs-agents-secret-release.md`), or run `./vendor/bridge --cli` in a terminal view and let the user type.
   Keep the bridge's local IMAP password in `data/` (mode 0600) or the Keychain.
5. The mail server reads IMAP from 127.0.0.1:1143 and publishes new mail on channel `mail/new`.

## Expose a web app (dev servers, dashboards, media servers)
Declare the port on the service: `"ports": [{ "port": 5173, "expose": "view" }]`. An html view loads it with
`<iframe src="oasvc://<module>/5173/">` (HTTP through the kernel; redirects to 127.0.0.1 are rewritten). Use relative
URLs in that app. WebSockets (HMR, live apps) need the gateway: `const u = await openapp.host("service.url", { port:
5173 })` → `u.http`, `u.ws`. `"expose": "lan"` also shares the port on the user's network after they allow it on a card
(Bonjour name = the module name); tell the user why first.

## Terminal (PTY + xterm.js)
`pty.open { module, command?, cols, rows, cwd? }` → `{ id, topic, input }` starts `bash -l` in /workspace (or your
command) on a real terminal in the VM. Output: binary messages on `topic`, then `{ "type": "exit", "code" }`. Input:
publish keystrokes on `input`. `pty.resize { id, cols, rows }`, `pty.write { id, data }`, `pty.close { id }`, host data
`ptys`. Put xterm.js in the module (`npm i @xterm/xterm` at the root; copy `lib/xterm.js` and `css/xterm.css` to
`modules/<id>/views/vendor/`):
```html
<link rel="stylesheet" href="vendor/xterm.css"><div id="t" style="height:100vh"></div>
<script src="vendor/xterm.js"></script>
<script>(async () => {
  const term = new Terminal({ fontSize: 13 }); term.open(document.getElementById("t"));
  const p = await openapp.host("pty.open", { cols: term.cols, rows: term.rows });
  openapp.subscribe(p.topic, (d) => d instanceof Uint8Array ? term.write(d) : d?.type === "exit" && term.write("\r\n[exited]\r\n"));
  term.onData((d) => openapp.publish(p.input, d));
  term.onResize(({ cols, rows }) => openapp.host("pty.resize", { id: p.id, cols, rows }));
})();</script>
```

## Linux GUI apps (virtual display)
`"display": { "command": ["xterm"], "width": 1280, "height": 800 }` runs the app on Xvfb with x11vnc + noVNC (service
`display`, restarted if the app exits). Install the app with `"apt"`. Show it in an html view:
```html
<iframe id="d" style="border:0;width:100%;height:100vh"></iframe>
<script>openapp.host("display.url", {}).then((d) => { document.getElementById("d").src = d.page; });</script>
```
`d.page` = noVNC (autoconnect, scaled); `d.ws` = the raw VNC WebSocket (`RFB` from `d.rfb_module`).

## Activation, idle stop, resources
Modules start lazily: `"activation": ["onView", "onAction"]` is the default (also `onStart`, `onChannel`, `onJob`).
Status `idle` = views shown, server not started; the first action or visible view starts it. A server with no visible
view, action or outside subscriber for 10 min goes back to `idle` (a service with `"restart": "always"` stays). Keep
state in `data/`, not only in memory. CPU %, RSS and disk per module are in `openapp_describe → resources` and host
data `{ "$host": "resources" }`; set `"budget": { "cpu": 50, "rss_mb": 512 }` if the default (90 % / 2 GB) does not
fit. `/safe` (user) stops all modules. `openapp_describe → primitives` lists every kernel primitive with an example.
