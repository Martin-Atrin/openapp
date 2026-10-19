# Testing

One command: `node scripts/test.ts [--vm] [--gui]` (or `pnpm test --vm --gui`). It runs clippy (warnings fail)
and `cargo test`, and optionally the VM suite and the GUI suite, then prints a summary with times.

| What | Command (in `src-tauri/`) | Needs |
| --- | --- | --- |
| Lint | `cargo clippy --all-targets` (0 warnings) | |
| Unit + host suite + models | `cargo test` | macOS, node + npm, swiftc, Apple Intelligence (on-device checks) |
| VM suite | `cargo test --test vm_suite -- --ignored` | the guest image tar (below), network for Codex / Pi / apt |
| GUI suite | `cargo run --example gui_shell` | a logged-in GUI session, Vite on 5173 (`pnpm dev`) |

## Layout

- `src-tauri/tests/kernel_host.rs`: the kernel without a VM: MCP, sessions, providers, undo, audio
  (`kernel_smoke`), §20 connect (`connect_smoke`), §18 macOS services + host modules (`macos_smoke`), and the host
  halves of the channels (`channels_host`) and wave-2 (`wave2_host`) cases, agent-set native shell state without a
  window (`shell_headless`).
- `src-tauri/tests/vm_suite.rs` (`#[ignore]`): `vm_smoke`, `services_smoke`, `pi_smoke`, `channels_vm`, `wave2_vm`,
  `socks_vm` (SOCKS5 egress: cards allow/deny/IP literal, socks5h, curl `$ALL_PROXY`, proxychains4, `"net": "socks"`,
  sockets removed on shutdown), `bulk_vm` (3 MiB uploads and downloads through both egress routes, a 1 MiB MCP
  request: the framed vsock relay).
- `src-tauri/tests/models.rs`: §23 model discovery against fake Codex / Pi processes (`tests/fakes`).
- `src-tauri/tests/support/`: the shared sink (`Collect`), `check!`, `wait_for`, card auto-answers, scratch dirs,
  the scratch VM home, process isolation; `fixtures.rs` holds module sources shared by host and VM halves.
- `src-tauri/examples/gui_shell.rs`: desired shell state applied on attach, native shell (windows, menus, tray, shortcuts, notifications, clipboard,
  deep links, files, folder grants + VM mount) and web control (child WKWebView, download capture).
- `src-tauri/examples/dev/`: `acp_smoke`, `codex_smoke` (real provider logins), `mcp_serve`, `contract_dump`.
- `src-tauri/examples/eval.rs` (+ `examples/eval/`): agent evaluation runs (not tests), one scripted user
  (`eval/driver.rs`): `cargo run --example eval -- probes --harness codex|pi [--only E1,E3] [--dry-run] …` writes
  `eval/probes/LOOP-<n>.md` (`eval/probes/PROBES.md`); `cargo run --example eval -- bakeoff` writes
  `eval/bakeoff/RESULTS-<date>.md`. `examples/eval/agent_run.rs` is the smoke helper the dev examples and
  `tests/models.rs` share.

## Isolation and cleanup

Each suite case runs in its own child process of the test binary with its own scratch workspace, state home,
host-module build dir, Keychain prefix (`com.polygonface.openapp.test-<case>-<pid>`) and env, so cases with
different settings run in parallel and nothing touches `~/.openapp`. After the case the harness deletes the scratch
dirs and every Keychain item recorded in the scratch state, also when the case fails.

- `cargo test --test kernel_host -- --nocapture` streams each case's `PASS` lines live; one case:
  `cargo test --test kernel_host connect_smoke`.
- `OPENAPP_TEST_KEEP=1` keeps the scratch dirs of a run for inspection.
- VM cases, and cases that write Keychain items, run one at a time. Each VM case gets `/tmp/oavm-<case>-<pid>` with the guest image copied in (an APFS clone)
  from `~/.openapp/vm/images/openapp-guest-<version>.tar`, or from `OPENAPP_TEST_GUEST_TAR`. The VM runtime
  comes from `OPENAPP_MSB_DIR` or `src-tauri/vm/runtime` (gitignored; `node scripts/fetch-runtime.ts` downloads
  microsandbox 0.7.8 `msb` + `libkrunfw` for darwin-arm64 from the official release and checks the pinned sha256).
- Optional parts: `OPENAPP_SMOKE_NO_AI=1` (skip the on-device agent run), `OPENAPP_SMOKE_CODEX_PROXY=1`,
  `OPENAPP_SMOKE_LAN=1`, `OPENAPP_SMOKE_PI_FAKE_KEY=0`, `OPENAPP_SMOKE_PI_TOOLS=0`, `OPENAPP_SMOKE_NO_VM=1`
  (gui_shell without the VM mount part).
