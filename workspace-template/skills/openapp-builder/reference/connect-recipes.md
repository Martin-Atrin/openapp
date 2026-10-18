# Connect a service: details and recipes

Credentials never enter the VM. The kernel keeps them in the Mac's Keychain and adds them to requests it makes for you.

## Pick the auth path
Ask the user in one question card: an API token or app password (`openapp_secret_request`), OAuth in the browser
(`openapp_oauth_authorize`), or a host module that uses the Mac's own account (Mail, Calendar, Contacts).

## Secrets and requests
- `openapp_secret_request {name, label, kind, help, url?, hosts?}` (kind `token` | `password` | `oauth-client`): a
  trusted kernel card with a masked field and a link to where the user creates it. `hosts` limits where the secret may
  be sent. `openapp_secrets` lists names; `openapp_secret_delete` removes one.
- `openapp_net_fetch {url, method, headers, json|body, auth: {secret, scheme?}}` → `{status, headers, body, json}`.
  Scheme: `bearer` (default for tokens and OAuth), `basic` (passwords; `username`), `header:<Name>`, `query:<param>`.
  OAuth tokens refresh by themselves.
- From a module server: `POST $OPENAPP_HOST_URL/net.fetch` with `Authorization: Bearer $OPENAPP_HOST_TOKEN` and the same
  JSON (also `/net.imap`, `/secret.request`, `/oauth.authorize`).
- Mail: `openapp_net_imap {host: "imap.gmail.com", secret, username, op: list|search|fetch, ...}` (read-only).
- OAuth: an `oauth-client` secret (pasted with `kind: "oauth-client"`, or captured from a web-view download) →
  `openapp_oauth_authorize {client, scopes, provider?, token?}` (presets: google, github, microsoft, slack, notion,
  dropbox, spotify) → `{token}`; use the token name in `auth.secret`. GitHub/Microsoft also have `openapp_oauth_device`.
- Host data `{"$host":"secrets"}` lists connected accounts (a list with delete actions) for a settings panel.

## Non-HTTP protocols from the VM (SOCKS5)
HTTP(S) uses `HTTP(S)_PROXY`. Everything else over TCP (IMAP/SMTP, MTProto, MQTT, XMPP, raw sockets) goes through the
kernel's SOCKS5 proxy: every VM process has `OPENAPP_SOCKS_URL=socks5h://<actor>:openapp@127.0.0.1:7803` (names resolve
on the Mac; same cards and grants as HTTP: "<module> wants to reach imap.gmail.com:993"; a raw IP gets its own card).
`ALL_PROXY` is not set (pass the URL explicitly).
- Libraries with a proxy option: pass it. Node `imapflow`: `proxy: process.env.OPENAPP_SOCKS_URL`; `nodemailer`:
  `proxy: process.env.OPENAPP_SOCKS_URL` (+ `socks` package); Telethon: `proxy=("socks5", "127.0.0.1", 7803, True,
  "module.<id>", "openapp")` (+ `python-socks`); TDLib: `addProxy {server: "127.0.0.1", port: 7803, type:
  proxyTypeSocks5 {username: "module.<id>", password: "openapp"}}`; curl: `-x "$OPENAPP_SOCKS_URL"`.
- No proxy option: `"net": "socks"` on the service (or `server`) runs it under proxychains4 (all TCP via SOCKS;
  127.0.0.1 stays local). Not for `"arch": "amd64"` programs.
- Credentials that should stay on the Mac: `openapp_net_imap` / `openapp_net_fetch` instead (no SOCKS needed).

## Guided consoles
When the user must do something in a provider's website (create an app, copy a key), do it inside OpenApp: a module
with a `web` view of the console (`profile` e.g. `google`), `openapp_host_action view.open {view, as: "sheet"}` (the web
view's placement is `present:<id>` from the result), then `openapp_guide {steps:[{title, detail}], current}` for the
strip above it. `openapp_web_read` gives text + element refs; navigate and fill freely. For anything that creates,
enables, accepts terms, grants access or costs money, `openapp_web_highlight {ref, note}` it and wait for the user's
**"Done: <step>"** message (`openapp_web_click` refuses those). Then move `current` on. Downloads from the web view are
captured: an OAuth client JSON becomes an `oauth-client` secret after the user confirms (check `openapp_secrets`);
other files land in `modules/<id>/data/downloads/`. Close the sheet and clear the guide when finished.

## Recipe: a Google API (Gmail, Calendar, Drive)
1. One question card with two paths: **A** "Your own Google Cloud OAuth client — about 3 minutes, full API" and
   **B** "IMAP + app password — mail only" (B: `openapp_secret_request {name: "gmail-app-password", kind: "password",
   url: "https://myaccount.google.com/apppasswords", hosts: ["imap.gmail.com"]}` → `openapp_net_imap`; needs 2-step
   verification).
2. A: write `modules/google-console` with view `{ "kind": "web", "url": "https://console.cloud.google.com/", "profile":
   "google" }`, load it, `view.open as: "sheet"` (width ~760), and set the guide: create or select a project → enable
   the API (e.g. `console.cloud.google.com/apis/library/gmail.googleapis.com`) → OAuth consent screen / Google Auth
   Platform: External, Testing, add the user's address as a test user → Clients / Credentials → Create OAuth client ID
   → **Desktop app** → Download JSON. Navigate to each page yourself (`web.navigate`), pre-fill names ("OpenApp"),
   highlight the buttons the user clicks.
3. The download is captured as `google-client` (confirm with `openapp_secrets`). Close the sheet, clear the guide.
4. `openapp_oauth_authorize {client: "google-client", scopes: ["https://www.googleapis.com/auth/gmail.readonly"], token:
   "gmail"}` (the user approves in the browser; "unverified app" warning → Continue, it is their own app).
5. Connector module: tools (`list_messages`, `get_message`, …) calling `net.fetch` with `auth: {secret: "gmail"}` on
   `https://gmail.googleapis.com/gmail/v1/users/me/...`.
6. Tell the user: in Testing status Google expires the refresh token after 7 days (connect again, or publish the app).

## Recipe: Telegram as the user (their chats, not a bot)
"Read and reply to my chats" means the user's own account. A bot token only reaches chats the bot joined: do not build
it unless the user chose it on a question card.
1. Service `tg` (`onStart`, `restart: always`) running a user-account client: TDLib (`tdl` / `tdjson`), or an MTProto
   library (Telethon with `pip` in a venv under `data/`, or GramJS with npm). Session files live in `data/` (0600).
   MTProto is not HTTP: give the client the SOCKS proxy (see "Non-HTTP protocols") or set `"net": "socks"`.
2. API id + hash come from https://my.telegram.org (user only): guided console in a web view, then `secret_request`
   `tg-api-id` and `tg-api-hash` and `secret.release` them to the module as env vars (`jobs-agents-secret-release.md`).
3. Login view: phone number → code → (2FA password via a secret card, never chat). The view calls tools on your server,
   which talks to the service on 127.0.0.1; show each login state.
4. Chat list, history and send are tools on your server; new messages go on a channel (`tg/new`) so the view updates.
