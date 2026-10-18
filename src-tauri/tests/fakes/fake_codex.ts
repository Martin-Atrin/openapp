/**
 * Fake `codex app-server` for tests/models.rs (§23). JSONL JSON-RPC on stdio. Lists an obviously fake catalog
 * with `model/list` (two pages; one hidden model only with includeHidden) and logs every request it gets to
 * $OPENAPP_FAKE_LOG (one JSON line each) so the test can check which model/effort each turn ran on.
 * Turn text markers: "SLOW" = finish after 2.5 s; "REJECT" = fail like an account that may not use the model.
 */
import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";

const log = process.env.OPENAPP_FAKE_LOG;
const send = (m: unknown) => process.stdout.write(`${JSON.stringify(m)}\n`);
const note = (m: unknown) => {
  if (log) appendFileSync(log, `${JSON.stringify(m)}\n`);
};

const effort = (e: string) => ({ reasoningEffort: e, description: `fake ${e}` });
const model = (id: string, name: string, efforts: string[], def: string, extra: Record<string, unknown> = {}) => ({
  id, model: id, upgrade: null, upgradeInfo: null, availabilityNux: null, displayName: name, description: `${name} (fake)`, modelSpecialty: null,
  hidden: false, supportedReasoningEfforts: efforts.map(effort), defaultReasoningEffort: def, inputModalities: ["text"], supportsPersonality: false,
  multiAgentVersion: null, additionalSpeedTiers: [], serviceTiers: [], defaultServiceTier: null, availableAccessPrograms: null, isDefault: false, ...extra,
});
const PAGE1 = [model("fake-alpha", "Fake Alpha", ["low", "medium", "high", "xhigh"], "low"), model("fake-beta", "Fake Beta", ["low", "medium", "high"], "medium", { isDefault: true })];
const PAGE2 = [model("fake-gamma", "Fake Gamma", ["medium", "high", "xhigh", "ultra"], "high")];
const HIDDEN = model("fake-hidden", "Fake Hidden", ["low"], "low", { hidden: true });

let turn = 0;
createInterface({ input: process.stdin }).on("line", (line) => {
  let m: { id?: number; method?: string; params?: Record<string, unknown> };
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  if (!m.method || m.id === undefined) return;
  note({ method: m.method, params: m.params });
  const p = m.params ?? {};
  const reply = (result: unknown) => send({ id: m.id, result });
  switch (m.method) {
    case "initialize": return reply({ userAgent: "fake-codex/0.0.0" });
    case "skills/list": return reply({ data: [] });
    case "skills/extraRoots/set": return reply({});
    case "account/read": return reply({ account: { type: "chatgpt" }, requiresOpenaiAuth: true });
    case "model/list": {
      if (!p.cursor) return reply({ data: PAGE1, nextCursor: "page-2" });
      return reply({ data: p.includeHidden ? [...PAGE2, HIDDEN] : PAGE2, nextCursor: null });
    }
    case "thread/start": return reply({ thread: { id: "fake-thread-1" } });
    case "thread/resume": return reply({ thread: { id: String(p.threadId) } });
    case "turn/start": {
      const id = `fake-turn-${++turn}`;
      reply({ turn: { id } });
      const text = JSON.stringify(p.input ?? "");
      const threadId = String(p.threadId);
      const finish = () => {
        if (text.includes("REJECT")) {
          const msg = `The '${String(p.model)}' model is not supported when using Codex with a ChatGPT account.`;
          send({ method: "turn/completed", params: { threadId, turn: { id, status: "failed", error: { message: msg } } } });
          return;
        }
        send({ method: "item/agentMessage/delta", params: { threadId, itemId: `${id}-msg`, delta: `ran on ${String(p.model)} ${String(p.effort)}` } });
        send({ method: "turn/completed", params: { threadId, turn: { id, status: "completed" } } });
      };
      setTimeout(finish, text.includes("SLOW") ? 2500 : 600);
      return;
    }
    default: return send({ id: m.id, error: { code: -32601, message: `fake codex: ${m.method}` } });
  }
});
