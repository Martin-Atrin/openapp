/**
 * Pi sign-in helper for the app VM (contract §17). The kernel writes this file into the Pi agent dir and runs
 * `node openapp-login.ts <provider>` in the guest with `PI_CODING_AGENT_DIR` and `OPENAPP_PI_PKG` (the installed
 * `@earendil-works/pi-coding-agent` folder) set. It runs Pi's own OAuth flow (`ModelRuntime.login`), which stores
 * the credential in `<agent dir>/auth.json`, and talks to the kernel in JSONL:
 *
 *   stdout  {"event":"auth_url","url","instructions"?} | {"event":"device_code","userCode","verificationUri"}
 *           {"event":"info"|"progress","message"} | {"event":"prompt","id","type","message","options"?}
 *           {"event":"prompt_closed","id"} | {"event":"done","ok":true} | {"event":"done","ok":false,"error"}
 *   stdin   {"id","value"} | {"id","cancel":true} | {"cancel":true}
 *
 * Credentials are never printed.
 */
import { createInterface } from "node:readline";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const provider = process.argv[2];
const pkg = process.env.OPENAPP_PI_PKG;
const agentDir = process.env.PI_CODING_AGENT_DIR;

function out(v: Record<string, unknown>): void {
  process.stdout.write(`${JSON.stringify(v)}\n`);
}

const waiting = new Map<number, { resolve: (v: string) => void; reject: (e: Error) => void }>();
const abort = new AbortController();
let nextId = 1;

// The kernel writes one JSON object per line; nothing it sends contains U+2028/U+2029.
createInterface({ input: process.stdin }).on("line", (line) => {
  let msg: { id?: number; value?: unknown; cancel?: boolean };
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.id === undefined) {
    if (msg.cancel) abort.abort();
    return;
  }
  const w = waiting.get(msg.id);
  if (!w) return;
  waiting.delete(msg.id);
  if (msg.cancel || typeof msg.value !== "string") w.reject(new Error("Login cancelled"));
  else w.resolve(msg.value);
});
process.stdin.on("end", () => abort.abort());

async function main(): Promise<void> {
  if (!provider || !pkg || !agentDir) throw new Error("usage: OPENAPP_PI_PKG=… PI_CODING_AGENT_DIR=… node openapp-login.ts <provider>");
  const mod = await import(pathToFileURL(join(pkg, "dist", "index.js")).href);
  const runtime = await mod.ModelRuntime.create({ refreshOnCreate: false, allowModelNetwork: false });
  const settings = mod.SettingsManager.create(process.cwd(), agentDir);
  await runtime.login(
    provider,
    "oauth",
    {
      signal: abort.signal,
      notify(event: Record<string, unknown>) {
        out({ ...event, event: event.type });
      },
      prompt(p: { type: string; message: string; options?: unknown; placeholder?: string; signal?: AbortSignal }) {
        const id = nextId++;
        return new Promise<string>((resolve, reject) => {
          waiting.set(id, { resolve, reject });
          out({ event: "prompt", id, type: p.type, message: p.message, options: p.options ?? null, placeholder: p.placeholder ?? null });
          p.signal?.addEventListener(
            "abort",
            () => {
              if (waiting.delete(id)) {
                out({ event: "prompt_closed", id });
                reject(new Error("Login cancelled"));
              }
            },
            { once: true },
          );
        });
      },
    },
    { getDeviceId: () => settings.getOrCreateDeviceId() },
  );
  if (typeof settings.flush === "function") await settings.flush();
}

main().then(
  () => {
    out({ event: "done", ok: true });
    process.exit(0);
  },
  (e: unknown) => {
    out({ event: "done", ok: false, error: e instanceof Error ? e.message : String(e) });
    process.exit(1);
  },
);
