/**
 * Fake `pi --mode rpc` for tests/models.rs (§23), run through OPENAPP_PI_CMD. Answers `/openapp-models` like the
 * OpenApp extension (a `notify` with `openapp.models:<json>`, obviously fake ids) and records `set_model` /
 * `set_thinking_level` in $OPENAPP_FAKE_LOG.
 */
import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";

const log = process.env.OPENAPP_FAKE_LOG;
const out = (m: unknown) => process.stdout.write(`${JSON.stringify(m)}\n`);
const note = (m: unknown) => {
  if (log) appendFileSync(log, `${JSON.stringify(m)}\n`);
};

let model = { provider: "fakeco", id: "fake-pi-startup" };
let level = "medium";
let ui = 0;
const DISCOVERY = {
  providers: [
    {
      provider: "fakeco", auth: "oauth", source: "chatgpt-account", liveCount: 3, unknown: ["fake-pi-unknown"], error: null,
      models: [
        { id: "fake-pi-one", name: "Fake Pi One", description: "fake", levels: ["off", "low", "medium", "high", "xhigh", "max"], liveEfforts: ["low", "medium", "high", "xhigh", "ultra"], defaultEffort: "low", hidden: false, isDefault: false },
        { id: "fake-pi-two", name: "Fake Pi Two", description: "fake", levels: ["off", "low", "medium", "high"], liveEfforts: ["low", "medium", "high"], defaultEffort: "medium", hidden: false, isDefault: true },
      ],
    },
  ],
};

createInterface({ input: process.stdin }).on("line", (line) => {
  let c: Record<string, unknown>;
  try {
    c = JSON.parse(line);
  } catch {
    return;
  }
  if (c.type === "extension_ui_response") return;
  note(c);
  const ok = (data?: unknown) => out({ type: "response", id: c.id, command: c.type, success: true, data });
  switch (c.type) {
    case "get_state": return ok({ model, thinkingLevel: level, isStreaming: false, sessionId: "fake-session", sessionFile: "/fake/session.jsonl" });
    case "new_session": return ok({ cancelled: false });
    case "switch_session": return ok({ cancelled: false });
    case "set_model": model = { provider: String(c.provider), id: String(c.modelId) }; return ok(model);
    case "set_thinking_level": level = String(c.level); return ok();
    case "get_available_models": return ok({ models: [] });
    case "abort": return ok();
    case "prompt": {
      const msg = String(c.message ?? "");
      if (msg.startsWith("/openapp-models")) {
        out({ type: "extension_ui_request", id: `ui-${++ui}`, method: "notify", message: `openapp.models:${JSON.stringify(DISCOVERY)}`, notifyType: "info" });
        return ok({ disposition: "handled" });
      }
      ok({ disposition: "accepted" });
      setTimeout(() => {
        out({ type: "message_start", message: { role: "assistant" } });
        out({ type: "message_update", assistantMessageEvent: { type: "text_delta", contentIndex: 0, delta: `ran on ${model.provider}/${model.id} ${level}` } });
        out({ type: "message_end", message: { role: "assistant", stopReason: "stop", content: [] } });
        out({ type: "agent_settled", aborted: false });
      }, msg.includes("SLOW") ? 2500 : 600);
      return;
    }
    default: return out({ type: "response", id: c.id, command: c.type, success: false, error: `fake pi: ${String(c.type)}` });
  }
});
