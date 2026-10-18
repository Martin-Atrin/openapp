# Host data, sessions, web views

## Host data
Bind host data in any spec prop with `{"$host": "<source>"}`. The shell fills it live. `openapp_describe` →
`host_data` lists every source (and `host_actions` every action).
- `sessions` → list items (`title`, relative-time `subtitle`, `active`, and an `action` that opens the session)
- `harnesses` → list items for the agent providers (the provider picker)
- `grants` → list items for what the user allowed (network hosts, microphone), each with a `grant.revoke` action
- `secrets` → connected accounts and keys (metadata only), each with a delete action
- `services`, `jobs`, `agent_runs`, `folders`, `resources`, `ptys` → status lists (see the other reference files)
- `session.active.title`, `agent.name`, `agent.status` → strings · `web:<placement>` → `{url, title, loading}`

## Sessions (conversations)
Sessions are **kernel-owned**: the kernel stores them, runs them and switches them. You build only the UI (e.g. a
sessions rail). Never store chats yourself.
```json
{ "type": "stack", "gap": 8, "children": [
  { "type": "button", "label": "New chat", "icon": "plus", "variant": "ghost", "action": { "host": "session.new" } },
  { "type": "list", "items": { "$host": "sessions" }, "empty": "No chats yet" } ] }
```
Host actions: `{"host":"session.new"}` · `{"host":"session.open","id":"s_..."}` ·
`{"host":"session.rename","id":"s_...","title":"..."}`. They also work from HTML views via `openapp.host(...)`.
Grants: `{"host":"grant.revoke","id":"network:github.com"}` (ids come from the `grants` host data). A grants panel is a
`list` with `items: {"$host": "grants"}`. `list` items accept `active: true` (highlighted).

## Web views (in-app browser surfaces)
```json
"views": { "gemini": { "kind": "web", "url": "https://gemini.google.com/app", "profile": "google" } }
```
- `profile` names a persistent browser profile (cookies, logins, storage). All web views with the same profile share
  it, so one Google sign-in serves several Google sites. Default profile: the module id.
- Place it like any view (a region, `main`, or a tab). Nothing may overlap it: the page draws above the app.
- Host actions: `{"host":"web.navigate","placement":"p_...","url":"https://..."}`, `web.back`, `web.forward`,
  `web.reload` (each with `placement`). Host data `{"$host":"web:<placement>"}` gives `{url, title, loading}` (e.g. for
  an address bar).
- To open several sites with buttons, use one web view and `web.navigate`, or one web view per tab in a `tabs` region.
- Read and use a web view yourself: `openapp_web_read/click/fill/highlight` (see `connect-recipes.md`).
