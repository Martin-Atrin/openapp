// Run the OpenApp test layout (see TESTING.md).
// Usage: node scripts/test.ts [--vm] [--gui] [--no-clippy]
//   default  cargo clippy --all-targets (warnings fail) + cargo test (unit, kernel_host, models)
//   --vm     + cargo test --test vm_suite -- --ignored (scratch VM homes; needs the guest image tar)
//   --gui    + cargo run --example gui_shell with the Vite dev server on 5173 (started and stopped here
//            unless one is already listening; needs a logged-in macOS GUI session)
// Exit 1 when a step fails; the summary lists every step with its time.

import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { connect } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const tauri = join(root, "src-tauri");
const flags = new Set(process.argv.slice(2).filter((a) => a !== "--"));
const known = new Set(["--vm", "--gui", "--no-clippy"]);
for (const f of flags) {
  if (!known.has(f)) {
    console.error(`unknown option ${f}\nusage: node scripts/test.ts [--vm] [--gui] [--no-clippy]`);
    process.exit(2);
  }
}

type Step = { name: string; ok: boolean; secs: number };
const steps: Step[] = [];

function step(name: string, cmd: string, args: string[], cwd = tauri): boolean {
  console.log(`\n=== ${name}: ${cmd} ${args.join(" ")}`);
  const t = Date.now();
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  const ok = r.status === 0;
  steps.push({ name, ok, secs: Math.round((Date.now() - t) / 1000) });
  return ok;
}

/** Vite listens on "localhost", which may be ::1 only: try both loopback addresses. */
function listening(port: number): Promise<boolean> {
  const one = (host: string) =>
    new Promise<boolean>((res) => {
      const s = connect({ host, port });
      s.once("connect", () => {
        s.destroy();
        res(true);
      });
      s.once("error", () => res(false));
    });
  return Promise.all([one("127.0.0.1"), one("::1")]).then((r) => r.some(Boolean));
}

async function waitFor(port: number, secs: number): Promise<boolean> {
  const end = Date.now() + secs * 1000;
  while (Date.now() < end) {
    if (await listening(port)) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
}

/** Start Vite on 5173 unless something already listens there; returns the process we started (to stop it). */
async function vite(): Promise<ChildProcess | null | "failed"> {
  if (await listening(5173)) {
    console.log("Vite: using the server already on 5173");
    return null;
  }
  const p = spawn("pnpm", ["exec", "vite", "--port", "5173", "--strictPort"], { cwd: root, stdio: "ignore", detached: true });
  if (!(await waitFor(5173, 60))) {
    if (p.pid) process.kill(-p.pid, "SIGTERM");
    return "failed";
  }
  console.log(`Vite: started on 5173 (pid ${p.pid})`);
  return p;
}

let ok = true;
if (!flags.has("--no-clippy")) ok = step("clippy", "cargo", ["clippy", "--all-targets", "--", "-D", "warnings"]) && ok;
ok = step("test", "cargo", ["test"]) && ok;
if (flags.has("--vm")) ok = step("vm", "cargo", ["test", "--test", "vm_suite", "--", "--ignored"]) && ok;
if (flags.has("--gui")) {
  const v = await vite();
  if (v === "failed") {
    steps.push({ name: "gui", ok: false, secs: 0 });
    console.error("Vite did not start on 5173");
    ok = false;
  } else {
    try {
      ok = step("gui", "cargo", ["run", "--example", "gui_shell"]) && ok;
    } finally {
      if (v?.pid) {
        process.kill(-v.pid, "SIGTERM");
        console.log("Vite: stopped");
      }
    }
  }
}

console.log("\n=== summary");
for (const s of steps) console.log(`${s.ok ? "ok  " : "FAIL"} ${s.name.padEnd(7)} ${s.secs} s`);
process.exit(ok ? 0 : 1);
