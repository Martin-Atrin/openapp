// Inspect a built OpenApp.app and assert every piece a portable install needs is present.
// Usage: node src-tauri/scripts/verify-bundle.ts [path/to/OpenApp.app]
// Default: src-tauri/target/release/bundle/macos/OpenApp.app. Exit 1 on any failure.
// Keep the piece list in sync with src-tauri/src/bundle.rs and tauri.conf.json bundle.macOS.files.

import { execFileSync } from "node:child_process";
import { accessSync, constants, existsSync, lstatSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const app = resolve(process.argv[2] ?? join(here, "..", "target", "release", "bundle", "macos", "OpenApp.app"));
const contents = join(app, "Contents");

type Piece = { label: string; rel: string; exec: boolean; arch?: boolean; entitlements?: string[] };

const plist = (key: string, file = join(contents, "Info.plist")): string | null => {
  try {
    return execFileSync("/usr/libexec/PlistBuddy", ["-c", `Print :${key}`, file], { encoding: "utf8" }).trim();
  } catch {
    return null;
  }
};

const mainExe = plist("CFBundleExecutable") ?? "openapp";
const pieces: Piece[] = [
  { label: "main executable", rel: `MacOS/${mainExe}`, exec: true, arch: true },
  { label: "msb (VM runtime)", rel: "MacOS/msb", exec: true, arch: true, entitlements: ["com.apple.security.hypervisor"] },
  { label: "libkrunfw (VM firmware)", rel: "Frameworks/libkrunfw.5.dylib", exec: false, arch: true },
  { label: "openapp-macos (host services)", rel: "MacOS/openapp-macos", exec: true, arch: true },
  { label: "OpenAppNotify.app (notifications)", rel: "Helpers/OpenAppNotify.app/Contents/MacOS/openapp-macos", exec: true, arch: true },
  { label: "OpenAppNotify.app Info.plist", rel: "Helpers/OpenAppNotify.app/Contents/Info.plist", exec: false },
  { label: "Info.plist", rel: "Info.plist", exec: false },
];

let failures = 0;
const fail = (msg: string): void => {
  failures++;
  console.log(`  FAIL ${msg}`);
};
const mb = (n: number): string => `${(n / 1e6).toFixed(1)} MB`;

const duBytes = (p: string): number => {
  const st = lstatSync(p);
  if (st.isSymbolicLink()) return 0;
  if (!st.isDirectory()) return st.size;
  return readdirSync(p).reduce((sum, name) => sum + duBytes(join(p, name)), 0);
};

if (!existsSync(contents)) {
  console.error(`verify-bundle: ${app} is not an app bundle (no Contents/)`);
  process.exit(1);
}
console.log(`verify-bundle: ${app}`);

for (const p of pieces) {
  const path = join(contents, p.rel);
  if (!existsSync(path) || !statSync(path).isFile()) {
    fail(`${p.label}: missing Contents/${p.rel}`);
    continue;
  }
  const size = statSync(path).size;
  let notes = "";
  if (p.exec) {
    try {
      accessSync(path, constants.X_OK);
    } catch {
      fail(`${p.label}: Contents/${p.rel} is not executable`);
    }
  }
  if (p.arch) {
    const archs = execFileSync("/usr/bin/lipo", ["-archs", path], { encoding: "utf8" }).trim();
    notes += ` [${archs}]`;
    if (!archs.split(/\s+/).includes("arm64")) fail(`${p.label}: no arm64 slice (${archs})`);
    try {
      execFileSync("/usr/bin/codesign", ["--verify", "--strict", path], { stdio: "pipe" });
      notes += " signed";
    } catch (e) {
      fail(`${p.label}: code signature invalid (${String((e as { stderr?: Buffer }).stderr ?? e).trim()})`);
    }
  }
  if (p.entitlements) {
    let ent = "";
    try {
      ent = execFileSync("/usr/bin/codesign", ["-d", "--entitlements", "-", "--xml", path], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
    } catch {
      /* reported below */
    }
    for (const e of p.entitlements) if (!ent.includes(e)) fail(`${p.label}: entitlement ${e} missing (the VM cannot start)`);
  }
  console.log(`  ok   ${p.label.padEnd(36)} Contents/${p.rel.padEnd(58)} ${mb(size).padStart(9)}${notes}`);
}

// msb must be the pinned runtime the SDK expects
try {
  const v = execFileSync(join(contents, "MacOS", "msb"), ["--version"], { encoding: "utf8" }).trim();
  if (v !== "msb 0.7.8") fail(`msb version is "${v}", expected "msb 0.7.8"`);
  else console.log(`  ok   msb version: ${v}`);
} catch (e) {
  fail(`msb --version failed: ${e}`);
}

const minOs = plist("LSMinimumSystemVersion");
if (minOs !== "26.0") fail(`LSMinimumSystemVersion is ${minOs}, expected 26.0`);
else console.log(`  ok   LSMinimumSystemVersion ${minOs}`);
const archPrio = plist("LSArchitecturePriority:0");
if (archPrio !== "arm64") fail(`LSArchitecturePriority is ${archPrio}, expected arm64`);
else console.log(`  ok   LSArchitecturePriority arm64`);
console.log(`  ok   version ${plist("CFBundleShortVersionString")} (${plist("CFBundleIdentifier")})`);

// nothing in the bundle may point back at the build machine
const buildRoot = resolve(here, "..", "..");
for (const rel of [`MacOS/${mainExe}`]) {
  const out = execFileSync("/usr/bin/strings", ["-a", join(contents, rel)], { encoding: "utf8", maxBuffer: 1 << 30 });
  const hits = out.split("\n").filter((l) => l.includes(`${buildRoot}/src-tauri/vm/runtime`) || l.includes(`${buildRoot}/spikes`) || l.includes("target/macos-services"));
  if (hits.length) fail(`${rel} embeds build-machine runtime paths: ${hits.slice(0, 3).join(" | ")}`);
  else console.log(`  ok   no build-machine runtime paths in ${rel}`);
}

console.log(`  total app size: ${mb(duBytes(app))}`);
if (failures) {
  console.log(`verify-bundle: ${failures} problem(s)`);
  process.exit(1);
}
console.log("verify-bundle: all pieces present");
