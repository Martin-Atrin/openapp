# OpenApp implementation notes

Condensed from the "implementation notes" sections of `spec/MVP-CONTRACT.md` (§17.1, §19.3, §20.4, §21.1, §23.1),
the 2026-10-15 VM spike report (folded in below; the spike code was removed 2026-10-17), the probe and bake-off
reports in `eval/` and the git history. Newest last. These notes explain why the
code is the way it is; the rules are in [CONTRACT.md](CONTRACT.md), open work in [ROADMAP.md](ROADMAP.md).

## 2026-10-15 — MVP kernel and shell (contract v0.1–v0.5, `db8bd0f`)

- Tauri 2 app: Rust kernel (`src-tauri/src`, no Tauri types in `kernel/`) + React shell (`src/`). Owner rule: the kernel
  never pre-builds features.
- ACP client hand-rolled with serde_json (the ACP crate churns). Codex runs through the native app-server
  (JSONL without the `jsonrpc` field; schema from `codex app-server generate-ts`, codex-cli 0.162).
- Live testing drove sessions (v0.2), the provider picker (v0.3), chat/build modes, kernel-shipped skills, questions,
  web surfaces, microphone as a host capability and visible command output (v0.4).
- Codex on the host needed `sandbox_workspace_write.network_access=true` (npm, model downloads); kept for the
  host test transport only.

## 2026-10-15 — App VM spike

Machine: Apple M5 Pro, 24 GB, macOS 26.6.2. Verdict: **microsandbox 0.7.8** (Apache-2.0; libkrun fork +
`libkrunfw.5.dylib`, GPL-2.0 kernel 6.12) passed all six goals; smolvm, raw libkrun and Virtualization.framework
were not needed.

| measure | result |
|---|---|
| cold boot from cached image to first process (10 runs) | median 208 ms |
| warm boot | median 186 ms |
| `exec true` in a running VM | median 0.79 ms, p95 1.12 ms |
| idle VM host RSS (2 GiB configured) | ~152 MB |
| guest writes 1000 small files to /workspace | 424 ms (stat virtualization off) |
| guest `git status`, 5k-file repo | 160–190 ms after the first index refresh |
| vsock NDJSON ping / exec-stdio ping | median 0.04 ms / 0.11 ms |
| Codex 0.162.1 npm install | 6.4 s, 389.5 MB |
| app-server `initialize` | 326–503 ms over exec stdio; 68–70 ms over vsock |

Gotchas that shaped the code: the VM home path must be short (unix sockets < 104 bytes → `~/.openapp/vm`); sqlx 0.9
needs Rust 1.94 (project-local `rust-toolchain.toml`); host → guest virtio-fs edits are stale for up to ~5 s (kernel
writes files the guest needs at once through the bridge); stat virtualization Strict writes xattrs and forces mode
0600 on host files, so `/workspace` uses Off and the image sets `safe.directory=*`; microsandbox's own domain
allowlist needs TLS interception for strict mode, so OpenApp uses no guest network device and a kernel proxy instead.

Also verified: a named ext4 volume survives removing the sandbox (`/opt/openapp` marker and Codex install kept,
`/root` reset); egress default-deny gave NXDOMAIN for unlisted hosts and refused IP literals. The release `msb` is
ad-hoc signed with `com.apple.security.hypervisor` + `cs.disable-library-validation` (re-sign with Developer ID for
notarization); `libkrunfw` is GPL-2.0 (kernel source offer needed when shipping); pin `msb` + `libkrunfw` together.
The runtime now lives in `src-tauri/vm/runtime` (`scripts/fetch-runtime.ts`).

## 2026-10-15 — VM integration (v0.6, `a32ec40`)

- Forwarders are `socat` from the image (no custom guest binary): guest :7801 → vsock → `run/<vm>/mcp.sock` → kernel
  HTTP; guest :7802 → vsock → `run/<vm>/proxy.sock` → egress proxy; host login callback ports → per connection
  `socat STDIO TCP:127.0.0.1:<port>` exec'd in the guest, open only during a sign-in.
- Proxy user = actor (`agent`, `module.<id>`, `kernel`); Codex HTTPS and its Responses websocket honour the proxy env.
- Consent timings: Allow once = 2 min, Deny = 1 min, concurrent connections share one card. Kernel cards survive
  the end of an agent turn.
- Codex sign-in uses the app-server API (`account/login/start`, `account/login/completed`, `account/read`), so no
  separate `codex login` process.

## 2026-10-15 — macOS bridge, host modules, presentations (v0.7, `232c550`)

- `openapp-macos` verified on the dev Mac: FoundationModels available (2.6 s cold reply), SpeechTranscriber with 28
  locales. Translation works without UI on macOS 26 for installed pairs (`TranslationSession(installedSource:target:)`).
  Extra methods `status`, `debug.render_text` (PNG for tests). A failed Swift build is a cargo warning.
- Host-module consent hashes relative paths + contents; the approved snapshot is copied outside the workspace and
  only that copy is compiled, so editing files after approval changes nothing until a new card is approved.
- Turns got `turnId`; `turn.undo` restores layout/theme from before the turn's first change (later changes are also
  reverted and reported) and unloads modules first loaded in the turn; files stay.
- Kernel state moved out of the workspace into `~/.openapp/state/<ws-id>/` (the one-time migration code was removed
  in Lean batch 1).

## 2026-10-17 — Agent framing and connect (v0.8, `7dde948`, `37d6eb5`)

- Framing: "the VM is the agent's world; missing means build it". Containment rules in every system prompt.
- Secrets: security-framework Keychain items; `net.fetch` with reqwest on the host, `net.imap` over tokio-rustls.
  A secret request waits ≤ 9 min per tool call (Codex `tool_timeout_sec` = 600), then returns `pending`.
- Web control evaluates a page helper per call (`eval_with_callback`), refs in `data-oa-ref`. Downloads go through
  WKDownload into `<state>/downloads-tmp/`. Notion uses HTTP Basic client auth for tokens.
- Tests: `tests/kernel_host.rs` connect_smoke (headless), `examples/gui_shell.rs` web part (hidden Tauri window + real WKWebView).

## 2026-10-17 — Pi harness (v0.8, `dada7af`)

- Pi 1.3.0 via npm into the VM volume; offline/telemetry env off; OpenApp owns `SYSTEM.md`, `mcp.json`,
  `settings.json`, the `openapp.ts` extension and the login helper (`workspace-template/pi/`).
- Pi has one active session, so turns are serialized and sessions map to Pi session files.
- Egress allowlist added `api.anthropic.com` and `platform.claude.com` for the Claude sign-in.
- Bake-off runner `examples/bakeoff.rs` (since 2026-10-17 `examples/eval.rs bakeoff`; Codex vs Pi, three prompts). The 2026-10-17 run skipped both providers:
  neither was signed in in that VM state (`eval/bakeoff/RESULTS-2026-10-17.md`).

## 2026-10-17 — Primitives G1–G10, wave 1 (v0.9, `bceef27`)

- Primitive registry so new primitives do not edit `kernel/`. Why each primitive exists: Forge
  `projects/openapp/notes/2026-10-17-buildability-matrix.md` (domains E1–E12).
- Guest moved from Alpine to **Debian trixie-slim** (glibc 2.41) for broad binary compatibility. The rootfs is fresh
  on every boot, so module packages are declared (`apt`) and reinstalled from the volume cache. binfmt_misc is
  unavailable, so `arch: amd64` wraps commands with `qemu-x86_64` explicitly. The image is built in a throwaway
  microsandbox VM (`node src-tauri/vm/guest-image/build.ts`), not Docker.
- Services run under `setsid -w` with pidfiles so the kernel can stop whole process groups.
- WKWebView custom schemes cannot carry WebSockets, hence the kernel port **gateway** next to `oasvc://`.
- Proton Mail Bridge: Proton's Linux packages are x86_64 only — build from source (Go) in the guest or run the amd64
  build under qemu (skill recipe).
- PTY resize uses `stty -F` in the guest: microsandbox 0.7.8's guest agent ignores the SDK resize message.
- A folder grant restarts the VM (~0.7 s) after the current agent turn.

## 2026-10-17 — Models discovered (v0.9.1, `952dfbc`)

- Incident: Pi defaulted to a hardcoded `gpt-5.5`, which a ChatGPT-account sign-in does not offer → every turn
  failed. Rule since then: no model ids in code.
- Pi discovery filters Pi's catalog by the account's live list fetched with Pi's own credentials (ChatGPT sign-in:
  `chatgpt.com/backend-api/codex/models`, then `api.openai.com/v1/models`; API key: `/v1/models`; Claude:
  `api.anthropic.com/v1/models`); no live list → Pi's catalog cross-checked against a cached Codex list.
- A provider error naming the model as unavailable drops it (`rejected`) and falls back with one system line.
- Test `src-tauri/tests/models.rs` (fake Codex app-server + fake Pi RPC via `OPENAPP_PI_CMD`; scans kernel, Pi
  templates and shell for model-id literals).

## 2026-10-17 — Portable .app and release gating (`d9f16f8`, `fa43491`)

- Every helper is found inside the `.app` (`bundle.rs`); first run downloads the guest image (sha256-checked, resume,
  progress on `kernel/vm-image`). Release builds run only Pi; the rest show "Coming soon" without probes.

## 2026-10-17 — Lean batches and probe loops (`135ed28`, `b1cd943`, `e7a9593`)

- Lean batch 1: dead code out, `kernel_gaps` made honest (entitlement gaps with workarounds), compact builder skill
  (SKILL.md + `reference/*.md`).
- Lean batch 2: `kernel/` split into `mod/chat/modules/present/actions/host/describe/grants`; **one consent path**
  (`consent.rs`); **atomic grant store** (one shape, temp + rename, one lock); shared RPC peer (`rpc.rs`) for ACP,
  Codex, Pi and the host sidecars.
- Probe runner `examples/probes.rs` (since 2026-10-17 `examples/eval.rs probes`; Pi, build mode, fresh workspace per probe, outcome-based checks). Loop 1: E11
  partial (4/5). Loop 2: E4 and E9 pass; E1–E3, E5–E8, E10, E12 partial (`eval/probes/LOOP-*.md`). Typical misses:
  services not reaching a running state, a secret release not requested, agents stopping before the end state.
- Loop 2 kernel fixes: guest image 0.9.1 adds `fd` (Pi's `find` tool; an existing `/opt/openapp` volume with Pi
  keeps working, the new image loads once, old images / markers / archives are pruned after the first boot);
  microphone + camera in html views (`primitives/media.rs`: `media.request`, `openapp.requestMedia`, WKWebView
  capture gate per module grant, fail closed); failed Pi / Codex tool chips carry the error text in `output`;
  `CONTRACT_VERSION` 0.10; test-only env switches `OPENAPP_TEST_ALLOW_LOOPBACK`, `OPENAPP_PI_CMD`, `OPENAPP_CODEX_CMD`
  are ignored in release builds. `tests/vm_suite.rs` pi_smoke §9 runs Pi's `find` / `read` tools against a fake local provider in the VM.

## 2026-10-17 — Contract consolidation

`spec/MVP-CONTRACT.md` (864 lines, appended over ten waves) replaced by `CONTRACT.md` (normative, generated API
tables), `ROADMAP.md` and this file. The old text stays in git history.

## 2026-10-17 — Loop 3 kernel fixes (probe loop 2: E5, E6, E10, E12 partial)

- K1 SOCKS5 egress: `egress::serve_socks` (RFC 1928 CONNECT, RFC 1929 user = actor, socks5h) shares `decide` and a
  new `connect_upstream` with the HTTP proxy; IP literals get their own card text. Guest `127.0.0.1:7803` over vsock
  (`run/<vm>/socks.sock`); opt-in `OPENAPP_SOCKS_URL=socks5h://<actor>:openapp@127.0.0.1:7803` in `vm::proxy_env`
  (no global `ALL_PROXY`: httpx refuses a SOCKS `ALL_PROXY` without `httpx[socks]`); `"net": "socks"` on services /
  servers → `vm::socks_wrap` (proxychains4 with a per-actor config). Guest image 0.9.2 adds proxychains4.
  Debug-only test hooks: `OPENAPP_TEST_EGRESS_LOCAL=1`, `OPENAPP_TEST_EGRESS_HOSTS=name=ip`.
- K2 agent host actions: explicit `AGENT_REGISTRY` / `USER_ONLY` lists in `primitives/connect.rs` (test keeps every
  action classified); `model.request` alias; native shell desired state (tray, menus, shortcuts, dock badge,
  windows) stored and applied on `shell_misc::attach`; headless calls return `{applied: false, pending: true}`;
  describe `native_shell`, host data `dock`, snapshot text section.
- K3 skill: live-data rule + example, non-HTTP protocols via SOCKS, host-module trigger list, secret host binding,
  `npm pkg set type=module` (E5/E6 servers failed on CommonJS `.ts`).
- K4 stale VM sockets: `owner.pid`, sockets closed first on quit, `vm::running_vms` checks owner + accept and
  removes stale sockets (probes, bake-off); `AppVm::start` refuses a run dir owned by another live process.
- Follow-up: SOCKS is opt-in (`OPENAPP_SOCKS_URL`, reserved for secret release; no global `ALL_PROXY`). Host modules
  embed an Info.plist (shared `USAGE_DESCRIPTIONS`, test against `Info.plist`); TCC uses the *responsible* process's
  plist, so outside the app bundle host modules are spawned with `responsibility_spawnattrs_setdisclaim`
  (`posix_spawn(POSIX_SPAWN_SETEXEC)` from `pre_exec`); E12's SIGABRT came from the probes harness being responsible.
