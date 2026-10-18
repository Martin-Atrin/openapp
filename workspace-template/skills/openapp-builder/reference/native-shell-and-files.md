# Native shell and the user's files

All are host actions (views: `{ "host": … }` / `openapp.host(…)`, servers: `POST $OPENAPP_HOST_URL/<name>`, you:
`openapp_host_action` with `"module": "<id>"` in args). They act for the calling module; registrations survive restarts.
The kernel stores tray, menus, shortcuts, dock badge and windows to open: without an app window the result says
`pending: true` and they apply when it opens (`openapp_describe` → `native_shell`). Panels the user answers
(`files.open`, `files.save`, `files.grant_folder`, `print`, `share`) belong on a control in a view.

## Windows, menus, tray, shortcuts, notifications
- **Window** (editors, games, players): `{ "host": "window.open", "view": "daw/editor", "title": "Mini DAW",
  "fullscreen": true }` → `{ windowId }`; also `window.close|focus|fullscreen { id? }`, `alwaysOnTop`, `width/height`.
  The view fills the window (placement `window:<id>`); html views there may use pointer lock, gamepad, fullscreen.
- **Menu bar**: `{ "host": "menu.set", "menus": [{ "title": "Track", "items": [{ "label": "Play", "shortcut": "Space",
  "action": { "tool": "play" } }, "separator", { "label": "Loop", "checked": true, "action": { "tool": "loop" } }] }] }`.
  Merged after OpenApp/Edit/View; `[]` removes yours. Conflicts come back in `conflicts`.
- **Shortcuts**: `{ "host": "shortcut.register", "accelerator": "Space", "action": { "tool": "play_pause" } }` works in
  every OpenApp window (not while typing unless it has Cmd/Ctrl). `"global": true` also works in the background (the
  user allows it once; e.g. `MediaPlayPause`). Cmd+Q/C/V/X/Z/W/H/M are OpenApp's.
- **Tray / menu-bar item**: `{ "host": "tray.set", "icon": "music", "title": "▶ Song", "menu": [{ "label":
  "Play/Pause", "action": { "tool": "toggle" } }], "action"?: … }` (lucide name or an image path); `tray.clear`. With a
  tray item, closing the main window keeps OpenApp running. Update `title` from your server.
- **Notifications**: `{ "host": "notify", "title": "Render done", "body": "mix.wav", "actions": [{ "id": "open",
  "label": "Open" }], "action": { "tool": "on_notification" } }` → your tool gets `{ actionId }` ("default" = click).
  If macOS refuses buttons for OpenApp, a plain notification is shown (result `via`).
- `dock.badge { text|null }` · `clipboard.write { text | image }` · `clipboard.read` (asks once) ·
  `print { view | url | path }` · `share { path | url | text }` (share sheet) · `media.request { audio?, video? }`
  (asks once per module and kind; in an html view use `openapp.requestMedia({ video: true })`, see host-modules.md).
- **Deep links**: manifest `"deeplinks": ["open", { "prefix": "play", "action": { "tool": "play" } }]` →
  `openapp://<module>/open/x?y=1` calls tool `on_deeplink { url, path, query }` (default) and is published on
  `kernel/deeplink`. **Open with OpenApp**: `"opens": [".eml", "audio/*"]` → tool `on_open { file }`.
- **Drag and drop**: spec node `{ "type": "dropzone", "label": "Drop audio", "action": { "tool": "import" } }` (gets
  `{ files }`), html: `openapp.onDrop(files => …)`; drag out: `openapp.dragOut({ path })` in a mousedown handler.
  Dropped/opened files are copied into `data/inbox/` (refs `{ path, name, size, mime }`; `path` works in the VM and for
  host services).

## The user's files (folder grants, open/save)
- `files.open { types?: [".md", "image/*"], multiple?, directory? }` → `{ files: [ref] }` (copies into `data/inbox/`;
  `directory: true` = a folder grant). `files.save { suggestedName, text | b64 | path, directory?: "~/Music" }` →
  `{ path }` (Save panel). `files.reveal { path }` (Finder), `files.quicklook { path }`.
- Real folders: `{ "host": "files.grant_folder", "purpose": "Save your songs", "name": "music", "suggested": "~/Music" }`
  → the user picks a folder → it appears in the VM at `/mnt/music` (read/write; the VM restarts in about 1 s and your
  server is restarted — keep state in files). Host services accept `/mnt/music/…` paths too. Host data
  `{ "$host": "folders" }` lists grants (item action revokes; `files.revoke_folder { name }`).
- Example (an editor app): `window.open` the editor full screen, `shortcut.register` Space = play,
  `files.grant_folder` ~/Music once, then write straight into `/mnt/music/<name>.wav`; or a one-off `files.save`.
