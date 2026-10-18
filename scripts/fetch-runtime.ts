// Fetch the App VM runtime (microsandbox 0.7.8 `msb` + `libkrunfw`, darwin-arm64) from the official
// microsandbox GitHub release into src-tauri/vm/runtime/{bin,lib}, the layout build.rs, tauri.conf.json
// (bundle.macOS.files) and the debug fallback in src-tauri/src/vm.rs expect. Every file is checked against a
// pinned sha256 (the same values scripts/licenses.ts pins); a mismatch deletes the download and exits 1.
//
// Usage: node scripts/fetch-runtime.ts [--out <dir>] [--force]
// Files that are already present with the right hash are kept (no download).

import { createHash } from "node:crypto";
import { chmodSync, createReadStream, existsSync, mkdirSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const VERSION = "0.7.8";
const BASE = `https://github.com/superradcompany/microsandbox/releases/download/v${VERSION}`;
const PIECES = [
  { asset: "msb-darwin-aarch64", rel: "bin/msb", mode: 0o755, sha256: "0995a03d0eaf46c7fadb28ea419be800def10f9152a12f0c03216bf4c8a0f06f" },
  { asset: "libkrunfw-darwin-aarch64.dylib", rel: "lib/libkrunfw.5.dylib", mode: 0o644, sha256: "723f7cd73be9bab21c0abf6a9b07b45a07d4cd178f1690250e392d55dfb236b4" },
];

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const argAt = process.argv.indexOf("--out");
const out = resolve(argAt > 0 && process.argv[argAt + 1] ? process.argv[argAt + 1] : join(root, "src-tauri", "vm", "runtime"));
const force = process.argv.includes("--force");

if (process.platform !== "darwin" || process.arch !== "arm64") {
  console.warn(`warning: this runtime is for darwin-arm64; this machine is ${process.platform}-${process.arch}`);
}

const sha256File = (p: string): Promise<string> =>
  new Promise((ok, fail) => {
    const h = createHash("sha256");
    createReadStream(p).on("data", (d) => h.update(d)).on("end", () => ok(h.digest("hex"))).on("error", fail);
  });

let failed = 0;
for (const p of PIECES) {
  const dest = join(out, p.rel);
  if (!force && existsSync(dest) && (await sha256File(dest)) === p.sha256) {
    console.log(`  ok   ${p.rel} (already present, sha256 matches)`);
    continue;
  }
  mkdirSync(dirname(dest), { recursive: true });
  const url = `${BASE}/${p.asset}`;
  const t0 = Date.now();
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    console.log(`  FAIL ${p.rel}: GET ${url} -> HTTP ${res.status}`);
    failed++;
    continue;
  }
  const bytes = Buffer.from(await res.arrayBuffer());
  const got = createHash("sha256").update(bytes).digest("hex");
  if (got !== p.sha256) {
    console.log(`  FAIL ${p.rel}: sha256 ${got} != pinned ${p.sha256} (nothing written)`);
    failed++;
    continue;
  }
  const tmp = `${dest}.part`;
  writeFileSync(tmp, bytes);
  chmodSync(tmp, p.mode);
  rmSync(dest, { force: true });
  renameSync(tmp, dest);
  console.log(`  ok   ${p.rel} <- ${p.asset} (${(bytes.length / 1048576).toFixed(1)} MB, ${((Date.now() - t0) / 1000).toFixed(1)} s, sha256 verified)`);
}

if (failed) {
  console.log(`fetch-runtime: ${failed} piece(s) failed`);
  process.exit(1);
}
console.log(`fetch-runtime: microsandbox ${VERSION} runtime ready in ${out}`);
