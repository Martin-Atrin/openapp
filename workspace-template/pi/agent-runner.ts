/**
 * OpenApp agent-module runner (contract §21 G10). The kernel writes this file into the app VM and runs
 * `node agent-runner.ts <job.json>` for one agent run with model "default" (Pi SDK, the VM's Pi sign-in).
 *
 * - Only the agent's allowed tools exist: Pi's built-in tools, skills, extensions, MCP servers and context files
 *   are all off (custom ResourceLoader, `noTools: "builtin"`). Each allowed tool is a custom tool that calls
 *   `POST $OPENAPP_HOST_URL/agent.tool { run, tool, args }` with this run's token; the kernel checks the
 *   tool again and runs it (module tool, host:<method> or openapp:<tool>).
 * - Budget: `turns` (assistant turns; the run is aborted after it) — the kernel also enforces minutes.
 * - Output: one JSON object per line on stdout: {type:"step", tool, ok} · {type:"result", text} · {type:"error", error}.
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

interface ToolSpec { name: string; target: string; description: string; inputSchema: Record<string, unknown> }
interface Job { run: string; instructions: string; input: string; tools: ToolSpec[]; turns: number; cwd: string; agentDir: string }

const out = (v: unknown) => process.stdout.write(JSON.stringify(v) + "\n");
const job: Job = JSON.parse(readFileSync(process.argv[2], "utf8"));
const PKG = process.env.OPENAPP_PI_PKG ?? "";
const HOST = process.env.OPENAPP_HOST_URL ?? "";
const TOKEN = process.env.OPENAPP_AGENT_TOKEN ?? "";

async function main(): Promise<void> {
  const pi = await import(pathToFileURL(`${PKG}/dist/index.js`).href);
  const req = createRequire(`${PKG}/package.json`);
  let Type: any = null;
  try {
    Type = (await import(pathToFileURL(req.resolve("typebox")).href)).Type;
  } catch {
    Type = null;
  }
  const schemaOf = (s: Record<string, unknown>) => {
    const base = s && typeof s === "object" ? s : { type: "object" };
    return Type?.Unsafe ? Type.Unsafe(base) : base;
  };
  const customTools = job.tools.map((t) => ({
    name: t.name,
    label: t.target,
    description: t.description || t.target,
    parameters: schemaOf(t.inputSchema),
    execute: async (_id: string, params: unknown) => {
      const r = await fetch(`${HOST}/agent.tool`, {
        method: "POST",
        headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
        body: JSON.stringify({ run: job.run, tool: t.target, args: params ?? {} }),
      });
      const body = await r.text();
      out({ type: "step", tool: t.target, ok: r.ok });
      return { content: [{ type: "text", text: body.slice(0, 20000) }], details: {} };
    },
  }));
  const resourceLoader = {
    getExtensions: () => ({ extensions: [], errors: [], runtime: pi.createExtensionRuntime() }),
    getSkills: () => ({ skills: [], diagnostics: [] }),
    getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }),
    getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => job.instructions,
    getSystemPromptSource: () => undefined,
    getAppendSystemPrompt: () => [],
    getAppendSystemPromptSources: () => [],
    extendResources: () => {},
    reload: async () => {},
  };
  const modelRuntime = await pi.ModelRuntime.create({ authPath: `${job.agentDir}/auth.json`, modelsPath: `${job.agentDir}/models.json`, allowModelNetwork: false });
  let model: unknown = undefined;
  try {
    const settings = JSON.parse(readFileSync(`${job.agentDir}/settings.json`, "utf8"));
    if (settings.defaultProvider && settings.defaultModel) model = modelRuntime.getModel(settings.defaultProvider, settings.defaultModel);
  } catch {
    // Pi picks its default
  }
  const { session } = await pi.createAgentSession({
    cwd: job.cwd,
    agentDir: job.agentDir,
    modelRuntime,
    ...(model ? { model } : {}),
    noTools: "builtin",
    customTools,
    resourceLoader,
    sessionManager: pi.SessionManager.inMemory(job.cwd),
    settingsManager: pi.SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: true, maxRetries: 1 } }),
  });
  let turns = 0;
  session.subscribe((e: { type: string }) => {
    if (e.type === "turn_end") {
      turns++;
      if (turns >= job.turns) {
        out({ type: "budget", turns });
        void session.abort();
      }
    }
  });
  try {
    await session.prompt(job.input || "Run your task.");
    out({ type: "result", text: session.getLastAssistantText() ?? "", turns });
  } finally {
    session.dispose();
  }
}

main().then(
  () => process.exit(0),
  (e) => {
    out({ type: "error", error: String(e?.message ?? e) });
    process.exit(1);
  },
);
