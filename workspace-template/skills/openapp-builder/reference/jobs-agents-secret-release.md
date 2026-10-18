# Jobs, agent modules, secret release

## Jobs and background (schedules, file watch, webhooks)
Never use setInterval/cron inside a server for user-visible schedules: declare jobs. The kernel runs them (also with
the window closed), activates the module, logs each run and shows them (`{ "$host": "jobs" }`, items have Run now).
```json
"jobs": [
  { "name": "sync", "schedule": { "every": "15m" }, "action": { "tool": "sync" } },
  { "name": "morning", "schedule": "0 8 * * *" },
  { "name": "import", "schedule": { "watch": "inbox/*.eml" }, "action": { "tool": "import" } },
  { "name": "hook", "schedule": { "webhook": true }, "action": { "tool": "on_hook" } }
]
```
- Cron is local time, 5 fields. Missed runs (Mac asleep, OpenApp closed) run **once** on wake. Watch/webhook runs pass
  `args.input` (`{ changed: [{path, change}] }` / `{ method, query, body }`). The webhook URL is in host data `jobs`
  (`url` on the Mac, `vm_url` from the VM); reaching it from the internet needs a tunnel the user allows.
- Runtime: `job.create { name, schedule, action }`, `job.update`, `job.delete`, `job.run_now { name }`, `job.list`
  (servers: `POST $OPENAPP_HOST_URL/job.create`). Runs publish `{ op: start|end, job, status, output_tail }` on
  `kernel/jobs`.
- Jobs, `"restart": "always"` services and host services keep OpenApp running when the window closes (menu-bar item).
  Ask with `background.request { reason }` when nothing else does. `app.login_item { enabled: true }` opens OpenApp at
  login (the user allows it once; it may need approval in System Settings).

## Agent modules (scoped agents with their own tools)
```json
"agents": [{ "name": "summary", "instructions": "AGENT.md", "model": "default",
  "tools": ["mail/list_unread", "today/today_events", "openapp:notify.post"],
  "budget": { "turns": 12, "minutes": 5 }, "triggers": ["morning"] }]
```
- The agent sees **only** `tools`: `<module>/<tool>` (or `<module>/*`), `host:<method>` (`host:ai.generate`,
  `host:<module>/<method>`), `openapp:<tool or host action>`. Anything else is refused. Tools run as this module.
- `model: "default"` = the VM agent with the user's sign-in (falls back to on-device when it is not signed in);
  `"on-device"` = the Mac's model (private, free, small: keep tasks short and tools few).
- Run: a job in `triggers`, `agent.run { agent, input?, wait? }`, `agent.cancel { run }`. Show runs with
  `{ "$host": "agent_runs" }` (status, steps) or subscribe to `kernel/agents`.
- Example, a morning summary: a mail connector module, a calendar host module, and a module `brief` with the agent
  above plus job `{ "name": "morning", "schedule": "0 8 * * *" }`. AGENT.md says what to read, how to summarize, and
  to notify with an action that opens the summary. Save the summary in `data/` so a view can show it.
- Light one-off steps inside a server can call `POST $OPENAPP_HOST_URL/ai.generate` instead of an agent.

## Secrets a program must hold itself (secret release)
Default stays `net.fetch` with `auth: {secret}`. Only when a daemon needs the credential itself (a mail bridge,
signal-cli, a DB server): `secret.release { name, module, as: "env:BRIDGE_PASSWORD" | "file:bridge/password" }`. The
user confirms every release. `env:` arrives at every start of the module's server and services; `file:` is a 0600 file
at `$OPENAPP_SECRETS_DIR/<path>` in the VM (tmpfs, never in the workspace). `secret.revoke { name, module }` takes it
back. `secret.generate { name, kind: password | ed25519 | x25519 | rsa-4096 }` creates one in the Keychain and returns
only the public part (`public_openssh`, `public_pem`) — e.g. a deploy key, then release it as a file.
- Pattern: `secret.request` the password (kind `password`) → `secret.release { as: "env:…" }` → the daemon logs in
  from the env (a 2FA code via a question card) → your server talks to the daemon on 127.0.0.1.
