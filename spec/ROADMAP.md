# OpenApp roadmap

Planned and unfinished work moved out of the contract on 2026-10-17 (contract v0.10, code `e7a9593`). Nothing here
is normative. When an item ships, move its rules into [CONTRACT.md](CONTRACT.md) and add a dated entry to
[IMPLEMENTATION-NOTES.md](IMPLEMENTATION-NOTES.md).

Status keys: **planned** (designed, no code) · **partial** (some code shipped; the rest is listed) · **in progress**
(work is under way now) · **blocked** (needs something outside the repo) · **vision** (direction, not scheduled).

## 1. Intelligent thread (old §14) — partial

The main chat thread is the one feature-rich surface the kernel ships.

| item | status | notes |
|---|---|---|
| Build card (title, live status line, inline cards, summary, Undo, Show details) | shipped | contract §13 |
| Command output in tool chips (command, cwd, output tail, exit code) | shipped | Codex + Pi |
| Full thread renderer: every Codex `ThreadItem` (plan checklist, fileChange + turn diff viewer, mcpToolCall args/result, webSearch, image items, sub-agent activity, context compaction, review mode); ACP updates mapped onto the same model | planned | types from `codex app-server generate-ts`, pinned per Codex version |
| `openapp_show {view: SpecNode \| {html}, title?, keep?}` — an interactive card in the current turn, persisted with the chat | planned | works in chat mode, no build needed |
| Card actions `{reply: "text"}` (buttons as answers) and `{reply_form: true}` (form values as JSON) | planned | |
| Catalog additions: `table` (sortable), `chart` (line \| bar \| area \| pie), `form` (text, number, select, date, checkbox, slider), `stat`, `tabs`, `code`, `diff`, `markdown` | planned | |
| "Keep this": promote an inline card to a module in build mode | planned | the thread becomes the incubator |
| MCP App widgets inline (`mcpToolCall.mcpAppUi.resourceUri`, `@modelcontextprotocol/ext-apps` AppBridge), sandboxed like html views | planned | |

## 2. OS generations, disk budget, system journal, starter packs (old §22) — planned

Owner framing (2026-10-17): the VM is "your OS, built by its lead agent from day 1". Owner constraint: **disk use
must stay bounded**; many generations must not bloat the drive.

- **Generations cost only what changed.** Generation = workspace git commit + kernel state snapshot (small JSON) +
  an OS delta only when system state changed. OS deltas are copy-on-write: preferred an APFS `clonefile` of the VM
  volume image; alternative an overlay upper-dir per generation. Decide by measuring bytes per typical install (spike).
- **Shared, content-addressed caches outside generations:** apt cache, one npm/pnpm store (no per-module
  `node_modules` copies), pip cache, model files by hash, release binaries. Counted once.
- **Retention (kernel GC):** keep the current, the last 10, one per day for 7 days, one per week for 4 weeks, and every
  pinned generation; prune and squash the rest.
- **Budget:** default min(20 GB, 5 % of the disk) for generations + caches + VM volume growth; user-adjustable in
  chat. Over budget → prune by retention → evict LRU cache entries → ask.
- **Low-disk mode** (free < 10 % or < 15 GB): no new OS deltas, aggressive pruning, warn user and agent; installs
  that would exceed the budget ask first.
- **Logs and research:** rotate and zstd-compress after 7 days; raw transcripts 30 days unless pinned; metrics kept.
- **API:** host data `storage` (by category, free disk, budget); actions `storage.prune`, `storage.budget {gb}`,
  `generation.pin/unpin {id, reason}`, `generation.list`, `generation.restore {id}`; describe reports the remaining
  budget before big installs; `turn.undo` becomes a restore of the generation before the turn; a generation that
  fails its health check (kernel + harness + services) falls back to the last good one and tells the user.
- **System journal:** `<workspace>/SYSTEM.md` kept by the lead agent (who the user is, what is installed and why,
  preferences, open threads; ≤ 200 lines, older detail in `system/journal/<yyyy-mm>.md`), read at session start,
  updated at the end of build turns. One lead agent owns the system; agent modules are its services.
- **First boot and starter packs:** after the provider is picked, one question card ("What do you use your laptop
  for?", multi-select) and offers of curated build plans, built as the first generation. Offers only.

Shipped pieces of the disk principle today: shared guest image with a 2 GiB headroom check, one sparse 20 GiB
volume, apt cache on the volume, 5 MiB service log cap, per-module disk sampling (no disk budget yet).

## 3. Signing, notarization, CSP — planned

| item | status | notes |
|---|---|---|
| Developer ID signing + hardened runtime for OpenApp.app | planned | `tauri.conf.json` `signingIdentity: "-"` (ad-hoc) today |
| Re-sign `msb` with Developer ID + `com.apple.security.hypervisor` + `cs.disable-library-validation` | planned | the release `msb` is ad-hoc signed; main binary needs no hypervisor entitlement |
| Notarization | planned | after signing |
| Notification buttons | blocked on signing | need a signed/installed app or the user allowing notifications; dev builds fall back to plain banners |
| Content Security Policy for the shell | planned | `app.security.csp` is `null` today |
| GPL-2.0 obligations for `libkrunfw.5.dylib` (offer matching kernel source/config) | planned | point to the libkrunfw release tag or mirror it |
| Guest image SBOM | planned | the image mixes licences |

## 4. Packaging leftovers — partial

| item | status | notes |
|---|---|---|
| Portable .app: bundled `msb`, `libkrunfw`, `openapp-macos`, `OpenAppNotify.app`; startup check | shipped | `bundle.rs` |
| Guest image release asset: fill `vm::GUEST_IMAGE_REPO` (still `Martin-Atrin/openapp`), publish `guest-0.9.2` | planned | until then a local archive or `OPENAPP_GUEST_IMAGE_URL` is needed |
| Guest image 0.9.2 (0.9.1: fd for Pi's find tool; 0.9.2: proxychains4 for SOCKS egress) | done | constants + sha256 in `vm.rs` 2026-10-17; published as a release asset |
| Bump `kernel::CONTRACT_VERSION` "0.3" → "0.10"; app version (0.1.0) policy | planned | describe still says 0.3 |
| Deep links and "Open with" | shipped, bundled builds only | `RunEvent::Opened` |
| `state.rs` header comment still names `<workspace>/.openapp/` | planned (doc fix in code) | |
| `views.rs` spec-catalog note lists only 7 `$host` sources (describe `host_data` has all) | planned (doc fix in code) | |

## 5. Honest kernel gaps that need Apple entitlements or are out of scope — blocked

Listed to the agent in `openapp_describe → kernel_gaps` with the workaround it builds instead:

| gap | needs | workaround today |
|---|---|---|
| HomeKit | HomeKit entitlement (signed build) | Shortcuts from a host module (`shortcuts run`), vendor or Home Assistant HTTP API |
| VPN / network extensions | NetworkExtension entitlement | userspace client (WireGuard, Tailscale) as a VM service, or the vendor app |
| Share and Finder extensions | appex targets | `opens` file types, drag and drop, deep links, `share` |
| Passkeys in web views | WebAuthn entitlement | password + code in the web view, or OAuth in the system browser |
| Other apps' stored secrets (cookies, Keychain items, mail passwords) | — (never) | `secret.request`, `oauth.authorize` |
| iframe of an outside web page | — | a web view |

## 6. Invariants not yet enforced — planned

From the canonical invariant list (Forge `projects/openapp/notes/2026-10-15-phase-2-depth-breadth.md` §2):

- **Module isolation in the VM:** module servers and services share `/workspace`, the guest loopback and the agent's
  user; a module can read another module's `data/`. Needs per-module users or namespaces (bubblewrap is in the image).
- **No confused deputy:** a module must not use its grant to read another module's data for the agent.
- **Untrusted-content labelling:** web-view text, connector replies and files enter agent context unmarked; a
  Confirmed action after untrusted input should need a fresh confirmation.
- **Transactions:** every in-app change as validate → build → stage → health-check → commit/roll back, with a git
  commit per change (today: layout/theme history + `turn.undo`; module code relies on the agent's git use).
- **Research controls:** research level, export with redaction and preview, purge — human-only.
- **Design system:** typed tokens (DTCG) and swappable UI kits behind `@openapp/ui`; today only theme tokens with
  contrast checks, and "use theme variables" is advice in the skill.
- **Host ACP harnesses** (claude, gemini, goose, opencode, custom) run on the Mac without the VM boundary; they are
  gated off in release builds. Next step: the same VM pattern as Pi/Codex.

## 7. Smaller open items

| item | status | notes |
|---|---|---|
| Microphone in html views: `media` primitive (`media.request {audio?, video?}`, `openapp.requestMedia`, WKWebView capture gate per module grant) | done (loop 2) | `primitives/media.rs`; decision unit-tested; no headless WKWebView test yet (needs a live capture check in the app) |
| Pi tool error capture in tool chips | in progress | 2026-10-17 |
| `net.smtp` | planned | send mail through provider APIs with `net.fetch` meanwhile |
| ACP `elicitation/create` → question card | planned | Codex and Pi questions work |
| ACP `session/load` history replay into the chat | planned | replayed history is dropped today |
| webm → m4a conversion for transcription | planned | webm is refused with a hint; the shell records AAC when WebKit can |
| Codex and ACP providers in release builds | planned | needs per-provider release review; `RELEASE_PROVIDERS` |
| Disk budget per module (`budget.disk_mb`) | planned | disk is sampled, not budgeted |
| Gate the `net.fetch` loopback test hook `OPENAPP_TEST_ALLOW_LOOPBACK` (net.rs) to debug builds | planned | honoured in release builds today; invariant 4 assumes it is unset |

## 8. Vision items from the canonical spec (not scheduled) — vision

WASM component backends (wasmtime, WASI 0.3) and a shared deno_core script host; A2A for remote agents; harness
certification levels (Connected → Tool-certified → Builder-certified); multi-agent rooms; phone access through a
remote kernel + PWA; signed module bundles for sharing (imports start with zero grants); Tauri 3 + CEF web surfaces
for uniform Chromium and CDP.
