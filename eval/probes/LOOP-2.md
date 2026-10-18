# Probe loop 2 — 2026-10-17 18:29

Runner: `src-tauri/examples/probes.rs` (harness `pi`, build mode, fresh workspace + kernel + session per probe). VM state: `~/.openapp/vm`. Per-probe timeout 20 min, up to 2 follow-ups ("Continue building; I'll do the human-only steps later."). Questions answered from the probe's list (fallback: first option), egress allowed only for the probe's hosts, listed consent cards allowed, secret cards cancelled. Folder grants are applied programmatically before the VM starts (fixture copies, not the real folders).

## Summary

| Probe | Status | Time (s) | Turns | Questions | Cards | Modules | Services | Shell cmds (outside) | Refusal phrases | Checks |
|---|---|---|---|---|---|---|---|---|---|---|
| E1 | **partial** | 468 | 3 | 0 | 2 | 1 | 1 | 11 (2) | 0 | 4/6 |
| E2 | **partial** | 318 | 3 | 0 | 0 | 1 | 0 | 2 (0) | 0 | 3/6 |
| E3 | **partial** | 376 | 3 | 0 | 0 | 1 | 1 | 0 (0) | 0 | 3/4 |
| E4 | **pass** | 113 | 1 | 0 | 0 | 1 | 1 | 0 (0) | 0 | 3/3 |
| E5 | **partial** | 373 | 3 | 1 | 2 | 1 | 0 | 2 (0) | 0 | 4/6 |
| E6 | **partial** | 357 | 3 | 1 | 2 | 1 | 0 | 3 (0) | 0 | 1/4 |
| E7 | **partial** | 352 | 3 | 0 | 2 | 1 | 0 | 0 (0) | 0 | 4/5 |
| E8 | **partial** | 252 | 3 | 0 | 0 | 1 | 0 | 1 (0) | 0 | 4/5 |
| E9 | **pass** | 186 | 2 | 1 | 3 | 4 | 0 | 1 (0) | 0 | 4/4 |
| E10 | **partial** | 267 | 3 | 0 | 0 | 1 | 0 | 1 (1) | 0 | 3/4 |
| E12 | **partial** | 412 | 3 | 1 | 4 | 1 | 0 | 2 (0) | 0 | 4/5 |

## Details

### E1 · Proton Mail via Bridge — partial

> Make me a Proton Mail client. Set up the Proton Mail Bridge in your VM, show me the login and anything it needs, then build an inbox view I can read mail in.

- Why: missing: bridge service running / waiting for login; secret release requested for bridge credentials
- End state wanted: module with a services entry running the bridge (health ok or waiting for login); a login UI wired to the bridge CLI; an inbox view bound to a live channel; secret release requested for the bridge credentials
- Human-only steps allowed: entering the Proton password / 2FA
- Checks:
  - [x] service entry for the Proton Bridge
  - [ ] bridge service running / waiting for login
  - [x] login UI (secret cards or a login view)
  - [x] bridge CLI wired (bridge --cli / grpc / login flow)
  - [x] inbox view bound to a live channel
  - [ ] secret release requested for bridge credentials
  - [~] Proton password / 2FA entered by the human (needs a human, not scored)
- Cards: egress×2
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: `proton.me`, `api.github.com` · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `proton-mail` (status: proton-mail=running)
- Services: `proton-mail/bridge`
- Tool calls: 36 (openapp MCP: 13) · shell commands: 11 · outside /workspace: 2
- Commands touching outside /workspace: `dpkg --add-architecture amd64 && apt-get update -qq && mkdir -p /workspace/modules/proton-mail/debs && cd /workspace/modules/proton-mail/debs && apt-g…`, `grep -R '^deb ' /etc/apt/sources.list* 2>/dev/null / head; apt-cache policy libc6:amd64 / head -15; apt-get -o APT::Architecture=amd64 -o APT::Archite…`
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G2: services (check failed: bridge service running / waiting for login)
  - G7: secret.release (check failed: secret release requested for bridge credentials)
- Final summary:

  > Built and opened the Proton Mail Bridge setup sheet. Proton Mail Bridge 3.27.1 is installed as an amd64 VM service with a local IMAP listener on port 1143; use **Start Bridge CLI**, sign in, then run `info` to see the local credentials needed by the **Inbox** panel.

### E2 · Music studio — partial

> Build a small music studio like GarageBand: tracks, record from my mic, a metronome, a MIDI keyboard instrument, and export to my Music folder.

- Why: missing: host module declared; host module uses AVAudioEngine; live meters over a channel
- End state wanted: host module service with AVAudioEngine; full-screen or sheet editor view with live meters over a channel; keyboard shortcut for play/stop; export path via files.save or folder grant
- Human-only steps allowed: approving consent cards, macOS mic prompt
- Checks:
  - [ ] host module declared
  - [ ] host module uses AVAudioEngine
  - [ ] CoreMIDI used (optional)
  - [x] editor view (sheet / window / fullscreen)
  - [ ] live meters over a channel
  - [x] keyboard shortcut for play/stop
  - [x] export via files.save or folder grant
  - [~] mic consent + macOS mic prompt (needs a human, not scored)
- Cards: —
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: `folder panel for Music replaced by a programmatic grant of /var/folders/lw/lnvs417x6654wh8s79skzx200000gn/T/openapp-probes/fixtures/Music (-> /mnt/mus…`
- Modules created: `studio` (status: studio=running)
- Services: —
- Tool calls: 21 (openapp MCP: 11) · shell commands: 2 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`
- Next primitive hints: 
  - G8: long-running host module (service: true) or the agent did not use a host module (check failed: host module declared)
  - G8: long-running host module (service: true) or the agent did not use a host module (check failed: host module uses AVAudioEngine)
  - G1: live channels (check failed: live meters over a channel)
- Final summary:

  > Continued and verified the Mini Studio build: the module is running cleanly with microphone tracks, metronome, playable/MIDI keyboard, track management, and Music-folder export wired up. The remaining steps are human-only permissions for microphone access and choosing the Music folder when you use those controls.

### E3 · Terminal, editor, preview — partial

> Add a terminal and a code editor for my workspace, and a preview of any web server I start.

- Why: missing: xterm.js in a view
- End state wanted: view with xterm.js bound to a PTY channel that echoes input; editor view; oasvc preview of a running dev server
- Human-only steps allowed: none
- Checks:
  - [ ] xterm.js in a view
  - [x] PTY channel (pty.open / <module>/pty/)
  - [~] PTY echoes typed input (needs a live run of the view) (needs a human, not scored)
  - [x] editor view (Monaco / CodeMirror / textarea)
  - [x] web server preview (oasvc:// or a service port with expose view)
- Cards: —
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `devstudio` (status: devstudio=running)
- Services: `devstudio/preview-proxy`
- Tool calls: 26 (openapp MCP: 11) · shell commands: 0 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G2: PTY + xterm view (check failed: xterm.js in a view)
- Final summary:

  > Added **Dev Studio** to the composer actions. Open it to use:  - Workspace file browser and code editor with open/save support - Integrated `/workspace` terminal - Web preview for local servers via the Preview URL field, including VM-hosted servers through the built-in preview proxy

### E4 · LibreOffice in a display — pass

> I need LibreOffice inside OpenApp to edit documents.

- Why: all required checks pass
- End state wanted: display service running LibreOffice; a view showing the remote display; a document opens
- Human-only steps allowed: none
- Checks:
  - [x] display declared in a manifest
  - [x] LibreOffice installed / started (apt or command)
  - [x] view showing the remote display (display.url / oasvc / vnc / xpra)
  - [~] a document opens in the display (needs a live view) (needs a human, not scored)
- Cards: —
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 0 
- Needs human / runner substitutions: —
- Modules created: `libreoffice` (status: libreoffice=running)
- Services: `libreoffice/display`
- Tool calls: 11 (openapp MCP: 6) · shell commands: 0 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: —
- Next primitive hints: —
- Final summary:

  > Built LibreOffice Writer inside OpenApp. Use the **Open LibreOffice Writer** button beside the chat composer to launch the editable Writer window in a sheet.

### E5 · Unified inbox (Gmail + IMAP) — partial

> Build me a unified inbox for my Gmail and my work IMAP account.

- Why: missing: channel for new mail; job for periodic sync
- End state wanted: connector modules; inbox view; channel for new mail; job for periodic sync; agent asked for credentials via secret cards (not chat)
- Human-only steps allowed: entering app passwords
- Checks:
  - [x] connector modules (>= 1 mail/imap module)
  - [x] inbox view
  - [ ] channel for new mail
  - [ ] job for periodic sync
  - [x] credentials requested via secret cards
  - [x] no credentials asked in chat
  - [~] app passwords entered by the human (needs a human, not scored)
- Cards: question×1, secret×1
- Questions: `How should the inbox connect to Gmail? OAuth is the recommended full Gmail API path; IMAP is simpler and read-only. -> IMAP + app password (matched)`, `What are your work IMAP server details? Please provide the hostname, port, encryption (SSL/TLS or STARTTLS), and username. Do not include th… -> Use y…`
- Question fallbacks (first option used): `What are your work IMAP server details? Please provide the hostname, port, encryption (SSL/TLS or STARTTLS), and username. Do not include th… -> free …`
- Secret cards cancelled: `gmail-app-password (password) hosts [String("imap.gmail.com")]`
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `unified-inbox` (status: unified-inbox=running)
- Services: —
- Tool calls: 27 (openapp MCP: 14) · shell commands: 2 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G1: live channels (check failed: channel for new mail)
  - G6: jobs registry (cron / watch) (check failed: job for periodic sync)
- Final summary:

  > Built a unified inbox module at `modules/unified-inbox/`.  It includes a composer button that opens a Gmail + Work IMAP inbox, unified search/filtering, sync support, and mailbox settings that keep passwords out of the app and use Keychain secret names. It currently shows sample messages until the two mailbox accounts are configured.

### E6 · Telegram client — partial

> Connect my Telegram so I can read and reply to chats here.

- Why: missing: service running TDLib / a Telegram client library; phone / QR login UI; chat list view bound to a channel
- End state wanted: TDLib or a Telegram client library running as a service; phone/QR login UI; chat list view bound to a channel; send works against the service
- Human-only steps allowed: entering the phone code
- Checks:
  - [ ] service running TDLib / a Telegram client library
  - [ ] phone / QR login UI
  - [ ] chat list view bound to a channel
  - [x] send path against the service
  - [~] phone code entered by the human; real send (needs a human, not scored)
- Cards: question×1, secret×1
- Questions: `Which Telegram connection should I build? A bot token connects chats where your bot is a member; a personal-account connection reads and rep… -> Bot t…`
- Question fallbacks (first option used): `Which Telegram connection should I build? A bot token connects chats where your bot is a member; a personal-account connection reads and rep… -> first…`
- Secret cards cancelled: `telegram-bot-token (token) hosts [String("api.telegram.org")]`
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `telegram` (status: telegram=running)
- Services: —
- Tool calls: 25 (openapp MCP: 9) · shell commands: 3 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G2: services (check failed: service running TDLib / a Telegram client library)
  - G1: live channels (check failed: chat list view bound to a channel)
- Final summary:

  > Added a Telegram connector module with:  - Secure bot-token connection flow via macOS Keychain - Telegram button beside the composer - Inbox sheet showing chats and messages - Refreshing incoming messages - Reply composer for sending messages - Persistent local chat state  Open Telegram from the button next to Send, then connect your bot using a token from BotFather.

### E7 · Photos grid and crop — partial

> Show my Photos library as a grid and let me crop and export a picture.

- Why: missing: thumbnails streamed (channel / b64)
- End state wanted: host module reading Photos; thumbnails streamed; crop view; export via files.save
- Human-only steps allowed: Photos permission prompt
- Checks:
  - [x] host module declared
  - [x] host module reads Photos (PhotoKit)
  - [ ] thumbnails streamed (channel / b64)
  - [x] crop view
  - [x] export via files.save
  - [~] Photos permission prompt (needs a human, not scored)
- Cards: host_module×2
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: `host_module: Photos wants to run code on your Mac [module "photos" imports ["AppKit","Foundation","Photos"] calls []] -> allow`, `host_module: Photos wants to run code on your Mac [module "photos" imports ["AppKit","Foundation","Photos"] calls []] -> allow`
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `photos` (status: photos=running)
- Services: —
- Tool calls: 27 (openapp MCP: 19) · shell commands: 0 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G1: live channels (check failed: thumbnails streamed (channel / b64))
- Final summary:

  > Built a Photos library tool in the **Photos** button beside the composer. It opens a grid of recent Photos, lets you select an image, adjust a centered crop, and export the cropped picture through the native save dialog.

### E8 · File manager and Markdown editor — partial

> Give me a file manager for my Documents folder and a Markdown editor that opens .md files from Finder.

- Why: missing: reveal in Finder (files.reveal)
- End state wanted: folder grant used; browser view; editor; manifest opens: [".md"]; reveal in Finder works
- Human-only steps allowed: folder grant panel
- Checks:
  - [x] folder grant used (/mnt/documents or files.grant_folder)
  - [x] file browser view
  - [x] Markdown editor
  - [x] manifest opens: [".md"]
  - [ ] reveal in Finder (files.reveal)
  - [~] folder grant panel (needs a human, not scored)
- Cards: —
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent asked in plain text)
- Needs human / runner substitutions: `folder panel for Documents replaced by a programmatic grant of /var/folders/lw/lnvs417x6654wh8s79skzx200000gn/T/openapp-probes/fixtures/Documents (-> …`
- Modules created: `documents` (status: documents=running)
- Services: —
- Tool calls: 19 (openapp MCP: 11) · shell commands: 1 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`
- Next primitive hints: 
  - G5: files.reveal (check failed: reveal in Finder (files.reveal))
  - agent still waits on the user after the follow-ups -> builder skill should say what to do without answers
- Final summary:

  > The Documents module is built and running. The remaining human-only step is granting access to the Mac’s Documents folder; once you do that from **Connect Documents**, browsing, editing, saving, and Finder `.md` opening will be active.

### E9 · Morning summary job — pass

> Every morning at 8, summarise my inbox and today's calendar and send me a notification.

- Why: all required checks pass
- End state wanted: job with a cron schedule; an agent (G10) with scoped tools; notification with an action that opens the summary
- Human-only steps allowed: Calendar permission prompt
- Checks:
  - [x] job with a cron schedule at 8
  - [x] agent module (manifest agents) with scoped tools
  - [x] EventKit host module (calendar)
  - [x] notification with an action that opens the summary
  - [~] Calendar permission prompt; a real 8:00 run (needs a human, not scored)
- Cards: egress×1, host_module×1, question×1
- Questions: `Which secure connection should the morning summary use for inbox access? -> Gmail OAuth (fallback first option)`
- Question fallbacks (first option used): `Which secure connection should the morning summary use for inbox access? -> first option "Gmail OAuth"`
- Secret cards cancelled: —
- Egress allowed: — · denied: `gmail.googleapis.com`
- Consent decisions: `host_module: Today wants to run code on your Mac [module "today" imports ["EventKit","Foundation"] calls []] -> allow`
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 1 (agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `brief`, `google-console`, `mail`, `today` (status: brief=running, google-console=running, mail=running, today=running)
- Services: —
- Tool calls: 32 (openapp MCP: 18) · shell commands: 1 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: —
- Next primitive hints: 
  - egress: hosts denied by the probe policy: gmail.googleapis.com -> add to the default allowlist or to this probe's approvals
- Final summary:

  > Built the daily 8:00 AM morning briefing in OpenApp: Gmail unread-mail connector, macOS Calendar access, scoped summary agent, notification delivery, and scheduled job. The Google OAuth setup remains open in the guided panel; after you complete it and allow Calendar access, the summaries will run automatically.

### E10 · Music player with menu-bar mini player — partial

> Make a music player for the files in my Music folder with a mini player in the menu bar.

- Why: missing: media keys / Now Playing via host module
- End state wanted: player view; tray mini player with play/pause; media keys or Now Playing via host module
- Human-only steps allowed: folder grant panel
- Checks:
  - [x] player view
  - [x] music folder granted / used (/mnt/music)
  - [x] tray mini player with play/pause (tray.set)
  - [ ] media keys / Now Playing via host module
  - [~] folder grant panel (needs a human, not scored)
- Cards: —
- Questions: —
- Question fallbacks (first option used): —
- Secret cards cancelled: —
- Egress allowed: — · denied: —
- Consent decisions: —
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: `folder panel for Music replaced by a programmatic grant of /var/folders/lw/lnvs417x6654wh8s79skzx200000gn/T/openapp-probes/fixtures/Music (-> /mnt/mus…`
- Modules created: `music-player` (status: music-player=running)
- Services: —
- Tool calls: 23 (openapp MCP: 14) · shell commands: 1 · outside /workspace: 1
- Commands touching outside /workspace: `mkdir -p /workspace/modules/music-player/views /workspace/modules/music-player/data && cat > /workspace/modules/music-player/openapp.json <<'EOF' {   …`
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`
- Next primitive hints: 
  - G8: long-running host module (service: true) or the agent did not use a host module (check failed: media keys / Now Playing via host module)
- Final summary:

  > Improved the music player so its menu-bar mini-player now controls real playback: Play/Pause actions are wired to the open player through a live channel. The updated player is open now and remains available from the music-note button beside the composer.

### E12 · Home Assistant lights and BLE — partial

> Connect my Home Assistant and show my lights; also find Bluetooth sensors near me.

- Why: missing: BLE results streamed (channel.publish)
- End state wanted: HA connector via net.fetch + secret; lights view with toggles; host module scanning BLE, results streamed
- Human-only steps allowed: entering the HA token
- Checks:
  - [x] HA connector via net.fetch
  - [x] HA token requested via a secret card
  - [x] lights view with toggles
  - [x] host module scanning BLE (CoreBluetooth)
  - [ ] BLE results streamed (channel.publish)
  - [~] HA token entered by the human; Bluetooth prompt (needs a human, not scored)
- Cards: egress×1, host_module×1, question×1, secret×1
- Questions: `What is your Home Assistant base URL (for example, http://homeassistant.local:8123 or https://ha.example.com)? I’ll then request a secure ac… -> Use y…`
- Question fallbacks (first option used): `What is your Home Assistant base URL (for example, http://homeassistant.local:8123 or https://ha.example.com)? I’ll then request a secure ac… -> free …`
- Secret cards cancelled: `home-assistant-token (token) hosts [String("homeassistant.local")]`
- Egress allowed: `homeassistant.local:8123` · denied: —
- Consent decisions: `host_module: Home Assistant & Bluetooth wants to run code on your Mac [module "ha-devices" imports ["CoreBluetooth","Foundation"] calls []] -> allow`
- Consent denied: —
- Harness approval cards (allowed once): —
- Follow-ups sent: 2 (agent stopped before the end state; agent stopped before the end state)
- Needs human / runner substitutions: —
- Modules created: `ha-devices` (status: ha-devices=running)
- Services: —
- Tool calls: 39 (openapp MCP: 21) · shell commands: 2 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`, `The turn failed: Connection error.`
- Next primitive hints: 
  - G1: live channels (check failed: BLE results streamed (channel.publish))
- Final summary:

  > Built a **Home Assistant & Bluetooth** control in the composer. Open it with the house/Wi‑Fi button to configure Home Assistant lights, toggle them, and scan for nearby Bluetooth LE sensors; Bluetooth scanning is working, while Home Assistant still needs the secure `home-assistant-token` added to Keychain.

## Suggested next primitive (all probes)

- G1: live channels — probes E2, E5, E6, E7, E12
- G8: long-running host module (service: true) or the agent did not use a host module — probes E2, E2, E10
- G2: services — probes E1, E6
- G2: PTY + xterm view — probes E3
- G5: files.reveal — probes E8
- G6: jobs registry (cron / watch) — probes E5
- G7: secret.release — probes E1
- agent still waits on the user after the follow-ups -> builder skill should say what to do without answers — probes E8
- egress: hosts denied by the probe policy: gmail.googleapis.com -> add to the default allowlist or to this probe's approvals — probes E9

Raw transcripts: `spikes/probes/raw/2/<id>.json` (chat, research log, describe, manifests, module sources).
