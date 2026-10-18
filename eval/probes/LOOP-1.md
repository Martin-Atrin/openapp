# Probe loop 1 — 2026-10-17 18:25

Runner: `src-tauri/examples/probes.rs` (harness `pi`, build mode, fresh workspace + kernel + session per probe). VM state: `~/.openapp/vm`. Per-probe timeout 20 min, up to 2 follow-ups ("Continue building; I'll do the human-only steps later."). Questions answered from the probe's list (fallback: first option), egress allowed only for the probe's hosts, listed consent cards allowed, secret cards cancelled. Folder grants are applied programmatically before the VM starts (fixture copies, not the real folders).

## Summary

| Probe | Status | Time (s) | Turns | Questions | Cards | Modules | Services | Shell cmds (outside) | Refusal phrases | Checks |
|---|---|---|---|---|---|---|---|---|---|---|
| E11 | **partial** | 217 | 3 | 0 | 0 | 1 | 0 | 1 (0) | 0 | 4/5 |

## Details

### E11 · WebGPU gamepad game — partial

> Build a small WebGPU game I can play full screen with a gamepad.

- Why: missing: WebGL fallback
- End state wanted: html view using WebGPU (fallback WebGL); window.open fullscreen; gamepad input handled
- Human-only steps allowed: none
- Checks:
  - [x] html view
  - [x] WebGPU (navigator.gpu)
  - [ ] WebGL fallback
  - [x] fullscreen via window.open / requestFullscreen
  - [x] gamepad input handled
  - [~] actually plays with a gamepad (needs a window) (needs a human, not scored)
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
- Modules created: `neon-drift` (status: neon-drift=running)
- Services: —
- Tool calls: 15 (openapp MCP: 7) · shell commands: 1 · outside /workspace: 0
- Commands touching outside /workspace: —
- Failed commands: —
- Refusal phrases: —
- Kernel gaps named by the agent: —
- Errors: `The turn failed: Connection error.`
- Next primitive hints: —
- Final summary:

  > Fixed and validated the Neon Drift game. WebGPU now renders on its own background canvas while gameplay renders on a separate overlay, so the gamepad/keyboard controls and visual effects work correctly in `modules/neon-drift/views/game.html`.

## Suggested next primitive (all probes)

No hints. Every probe passed or was skipped.

Raw transcripts: `spikes/probes/raw/1/<id>.json` (chat, research log, describe, manifests, module sources).
