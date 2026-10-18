/**
 * OpenApp extension for Pi (contract §12.4, §17). The kernel writes this file into the Pi agent dir
 * (`/opt/openapp/home/pi/extensions/openapp.ts`) when Pi starts in the app VM; do not edit the copy there.
 *
 * - `ask_user`: the question tool. It asks through `ctx.ui.input` with a marked title, so in RPC mode
 *   it reaches OpenApp as an `extension_ui_request`; the kernel shows a §12.4 question card and answers
 *   with `{questionId: string[]}` as JSON.
 * - `/openapp-check`: diagnostics for the smoke test (system prompt, tools, a question round trip).
 * - `/openapp-models` (§23): the models the signed-in account can use. Pi's available models per provider,
 *   filtered by the provider's live list fetched with Pi's own credentials (ChatGPT sign-in: the ChatGPT
 *   account list Codex uses, else `/v1/models`; API keys and Claude: `/v1/models`). Replies with one
 *   `notify` whose message is `openapp.models:<json>`. Tokens are never printed.
 * - No approval gates: the VM is the boundary. The kernel MCP server is attached by `mcp.json`.
 */
import type { ExtensionAPI, ExtensionUIContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

/** Title prefix the kernel recognises; the JSON after it is the question payload. */
const MARK = "openapp.ask:";

interface Question {
  id: string;
  header?: string;
  question: string;
  options?: { label: string; description?: string }[];
  allowOther?: boolean;
}

const QuestionSchema = Type.Object({
  id: Type.String({ description: "Stable id for this question, e.g. \"auth\"" }),
  header: Type.Optional(Type.String({ description: "Short title (3-6 words)" })),
  question: Type.String({ description: "The question, one or two sentences" }),
  options: Type.Optional(
    Type.Array(
      Type.Object({
        label: Type.String({ description: "Option label (1-5 words)" }),
        description: Type.Optional(Type.String({ description: "One sentence: what this option means" })),
      }),
      { description: "2-4 choices. Omit for a free-text answer." },
    ),
  ),
  allowOther: Type.Optional(Type.Boolean({ description: "Also allow a typed answer (default true)" })),
});

type Answers = Record<string, string[]>;

/** Ask through the RPC extension UI; undefined = dismissed. */
async function ask(ui: ExtensionUIContext, questions: Question[], signal?: AbortSignal): Promise<Answers | undefined> {
  const payload = questions.map((q) => ({
    id: q.id,
    header: q.header ?? null,
    question: q.question,
    options: q.options?.length ? q.options.map((o) => ({ label: o.label, description: o.description ?? "" })) : null,
    isOther: q.allowOther ?? true,
    isSecret: false,
  }));
  const raw = await ui.input(MARK + JSON.stringify({ questions: payload }), "", { signal });
  if (raw === undefined) return undefined;
  try {
    const v = JSON.parse(raw) as unknown;
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const out: Answers = {};
      for (const [k, a] of Object.entries(v as Record<string, unknown>)) {
        out[k] = Array.isArray(a) ? a.filter((x): x is string => typeof x === "string") : typeof a === "string" ? [a] : [];
      }
      return out;
    }
  } catch {
    // a plain string answers the first question
  }
  return { [questions[0]!.id]: [raw] };
}

function summary(questions: Question[], answers: Answers): string {
  return questions
    .map((q) => `${q.header || q.question}: ${(answers[q.id] ?? []).join(", ") || "(no answer)"}`)
    .join("\n");
}

// ---------- §23 model discovery ----------

/** Notify prefix of the `/openapp-models` reply; the JSON after it is the discovery result. */
const MODELS_MARK = "openapp.models:";
/** Pi thinking levels, lowest first (pi-ai `EXTENDED_THINKING_LEVELS`). */
const LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

interface PiModel {
  id: string;
  name?: string;
  provider: string;
  reasoning?: boolean;
  thinkingLevelMap?: Record<string, string | null | undefined>;
}
interface Registry {
  getAvailable(): PiModel[];
  isUsingOAuth(model: PiModel): boolean;
  getApiKeyForProvider(provider: string): Promise<string | undefined>;
  getProviderAuth(provider: string): Promise<{ auth?: { apiKey?: string; headers?: Record<string, string>; baseUrl?: string } } | undefined>;
}
/** One model of the account's live list (provider endpoint); efforts in the provider's vocabulary. */
interface Live {
  id: string;
  name?: string;
  description?: string;
  efforts?: string[];
  defaultEffort?: string;
  hidden?: boolean;
  isDefault?: boolean;
  /** creation time (ms) when the list has one: newer first for lists without an account order */
  created?: number;
}

/** pi-ai `getSupportedThinkingLevels`: the levels `set_thinking_level` accepts for this model. */
function levelsOf(m: PiModel): string[] {
  if (!m.reasoning) return ["off"];
  return LEVELS.filter((l) => {
    const mapped = m.thinkingLevelMap?.[l];
    if (mapped === null) return false;
    if (l === "xhigh" || l === "max") return mapped !== undefined;
    return true;
  });
}

/** `chatgpt_account_id` claim of a ChatGPT access token (the token itself is never printed). */
function accountId(token: string): string | undefined {
  try {
    const payload = JSON.parse(Buffer.from(token.split(".")[1] ?? "", "base64url").toString("utf8")) as Record<string, unknown>;
    const auth = payload["https://api.openai.com/auth"] as Record<string, unknown> | undefined;
    const id = auth?.chatgpt_account_id ?? payload.chatgpt_account_id;
    return typeof id === "string" ? id : undefined;
  } catch {
    return undefined;
  }
}

async function getJson(url: string, headers: Record<string, string>): Promise<unknown> {
  const r = await fetch(url, { headers: { accept: "application/json", ...headers }, signal: AbortSignal.timeout(20000) });
  if (!r.ok) throw new Error(`${new URL(url).host} ${r.status}`);
  return r.json();
}

const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);

/** ChatGPT account list (the backend Codex uses): `{ models: [{ slug, display_name, supported_reasoning_levels, … }] }`. */
async function codexBackend(token: string): Promise<Live[]> {
  const headers: Record<string, string> = { authorization: `Bearer ${token}` };
  const acct = accountId(token);
  if (acct) headers["chatgpt-account-id"] = acct;
  const ver = process.env.OPENAPP_CODEX_CLIENT_VERSION || "0.0.0";
  const v = (await getJson(`https://chatgpt.com/backend-api/codex/models?client_version=${encodeURIComponent(ver)}`, headers)) as { models?: Record<string, unknown>[] };
  const list = (v.models ?? []).slice().sort((a, b) => Number(a.priority ?? 0) - Number(b.priority ?? 0));
  return list.flatMap((m) => {
    const id = str(m.slug) ?? str(m.id) ?? str(m.model);
    if (!id) return [];
    const levels = (m.supported_reasoning_levels ?? m.supportedReasoningEfforts) as Record<string, unknown>[] | undefined;
    const vis = str(m.visibility);
    return [{
      id,
      name: str(m.display_name) ?? str(m.displayName),
      description: str(m.description),
      efforts: levels?.map((l) => str(l.effort) ?? str(l.reasoningEffort)).filter((x): x is string => !!x),
      defaultEffort: str(m.default_reasoning_level) ?? str(m.defaultReasoningEffort),
      hidden: m.hidden === true || (vis !== undefined && vis !== "list"),
      isDefault: m.is_default === true || m.isDefault === true,
    }];
  });
}

/** `GET /v1/models` (OpenAI-style `{ data: [{ id }] }`, Anthropic adds `display_name`). */
async function v1Models(url: string, headers: Record<string, string>): Promise<Live[]> {
  const v = (await getJson(url, headers)) as { data?: Record<string, unknown>[] };
  const created = (m: Record<string, unknown>) => (typeof m.created === "number" ? m.created * 1000 : str(m.created_at) ? Date.parse(str(m.created_at)!) : undefined);
  return (v.data ?? []).flatMap((m) => (str(m.id) ? [{ id: str(m.id)!, name: str(m.display_name), created: created(m) }] : []));
}

/** Live list of one provider with Pi's own credentials, or why it is not available. */
async function liveList(reg: Registry, provider: string, oauth: boolean): Promise<{ source: string; live?: Live[]; error?: string }> {
  const auth = (await reg.getProviderAuth(provider))?.auth;
  const key = auth?.apiKey ?? (await reg.getApiKeyForProvider(provider));
  if (!key) return { source: "catalog", error: "no credential" };
  const errors: string[] = [];
  const attempts: [string, () => Promise<Live[]>][] = [];
  if (provider === "openai") {
    if (oauth) attempts.push(["chatgpt-account", () => codexBackend(key)]);
    attempts.push(["openai-models", () => v1Models("https://api.openai.com/v1/models", { authorization: `Bearer ${key}`, ...(auth?.headers ?? {}) })]);
  } else if (provider === "anthropic") {
    const h: Record<string, string> = oauth
      ? { authorization: `Bearer ${key}`, "anthropic-beta": "oauth-2025-04-20" }
      : { "x-api-key": key };
    attempts.push(["anthropic-models", () => v1Models("https://api.anthropic.com/v1/models?limit=1000", { "anthropic-version": "2023-06-01", ...h, ...(auth?.headers ?? {}) })]);
  }
  for (const [source, f] of attempts) {
    try {
      const live = await f();
      if (live.length) return { source, live };
      errors.push(`${source}: empty`);
    } catch (e) {
      errors.push(`${source}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return { source: "catalog", error: errors.join("; ") || "no live list for this provider" };
}

/** Pi's available models per signed-in provider, filtered by the account's live list when one is reachable. */
async function discoverModels(reg: Registry): Promise<unknown> {
  const available = reg.getAvailable();
  const providers = [...new Set(available.map((m) => m.provider))];
  const out = [];
  for (const provider of providers) {
    const mine = available.filter((m) => m.provider === provider);
    const oauth = mine.some((m) => reg.isUsingOAuth(m));
    const r = await liveList(reg, provider, oauth);
    const byId = new Map(mine.map((m) => [m.id, m]));
    const known = r.live?.filter((l) => byId.has(l.id)) ?? [];
    if (r.source !== "chatgpt-account") {
      // no account order: models with more effort steps (newer reasoning models) first, then the newest
      known.sort((a, b) => levelsOf(byId.get(b.id)!).length - levelsOf(byId.get(a.id)!).length || (b.created ?? 0) - (a.created ?? 0));
    }
    const models = (r.live ? known : mine.map((m) => ({ id: m.id }) as Live)).map((l) => {
      const m = byId.get(l.id)!;
      return {
        id: m.id,
        name: l.name ?? m.name ?? m.id,
        description: l.description ?? "",
        levels: levelsOf(m),
        liveEfforts: l.efforts ?? null,
        defaultEffort: l.defaultEffort ?? null,
        hidden: l.hidden ?? false,
        isDefault: l.isDefault ?? false,
      };
    });
    out.push({
      provider,
      auth: oauth ? "oauth" : "api_key",
      source: r.live && known.length ? r.source : "catalog",
      liveCount: r.live?.length ?? null,
      unknown: r.live?.filter((l) => !byId.has(l.id)).map((l) => l.id) ?? [],
      error: r.error ?? (r.live && !known.length ? "the live list has no model Pi knows" : null),
      models: r.live && !known.length ? mine.map((m) => ({ id: m.id, name: m.name ?? m.id, description: "", levels: levelsOf(m), liveEfforts: null, defaultEffort: null, hidden: false, isDefault: false })) : models,
    });
  }
  return { providers: out };
}

export default function openapp(pi: ExtensionAPI) {
  pi.registerTool({
    name: "ask_user",
    label: "Ask the user",
    description:
      "Ask the user one to four questions in an OpenApp question card (choices and/or a typed answer). " +
      "This is the only way to ask the user anything in build mode. Never use it for passwords, tokens or keys " +
      "(use the openapp secret request tool for those).",
    promptSnippet: "Ask the user questions in an OpenApp question card (the question tool).",
    parameters: Type.Object({ questions: Type.Array(QuestionSchema, { minItems: 1, maxItems: 4 }) }),
    executionMode: "sequential",
    async execute(_id, params, signal, _onUpdate, ctx) {
      const questions = params.questions as Question[];
      if (!ctx.hasUI) throw new Error("No OpenApp user interface is attached");
      const answers = await ask(ctx.ui, questions, signal);
      if (!answers) {
        return {
          content: [{ type: "text", text: "The user closed the question without answering. Do not ask it again in this turn: say what you need and what the next step is." }],
          details: { answered: false },
        };
      }
      return { content: [{ type: "text", text: summary(questions, answers) }], details: { answered: true, answers } };
    },
  });

  pi.registerCommand("openapp-models", {
    description: "OpenApp model discovery (contract §23): what the signed-in account can use",
    handler: async (_args, ctx) => {
      let out: unknown;
      try {
        out = await discoverModels(ctx.modelRegistry as unknown as Registry);
      } catch (e) {
        out = { providers: [], error: e instanceof Error ? e.message : String(e) };
      }
      ctx.ui.notify(MODELS_MARK + JSON.stringify(out), "info");
    },
  });

  pi.registerCommand("openapp-check", {
    description: "OpenApp diagnostics (smoke test)",
    handler: async (args, ctx) => {
      const what = (args || "").trim();
      if (what === "ask") {
        const answers = await ask(ctx.ui, [
          { id: "smoke", header: "Smoke test", question: "Pick one", options: [{ label: "Alpha" }, { label: "Beta" }] },
        ]);
        ctx.ui.notify(`openapp-check ask ${JSON.stringify(answers ?? null)}`, "info");
        return;
      }
      if (what === "confirm") {
        const ok = await ctx.ui.confirm("Smoke test", "Allow the smoke test?");
        ctx.ui.notify(`openapp-check confirm ${JSON.stringify(ok)}`, "info");
        return;
      }
      const tools = pi.getAllTools().map((t) => t.name);
      const prompt = ctx.getSystemPrompt();
      ctx.ui.notify(
        `openapp-check info ${JSON.stringify({ tools, active: pi.getActiveTools(), promptHead: prompt.slice(0, 120), promptChars: prompt.length, model: ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : null })}`,
        "info",
      );
    },
  });
}
